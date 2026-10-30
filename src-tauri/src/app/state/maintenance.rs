//! 维护任务：批量任务入口、缩略图、空间优化、缩略图包回收、检查与导出。

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use parking_lot::Mutex;
use tauri::AppHandle;

use crate::{
    ai::{
        job::{AiJobKind, AiJobRequest},
        runtime::{GroupOptions, SimilarityGroupsResult},
    },
    app::{
        activity,
        batch_preview::{self, BatchTaskPreview, BatchTaskPreviewRequest},
        error::{AppError, AppResult},
        idle as idle_loop,
        tasks::{
            BatchTaskKind, BatchTaskRequest, ConflictPolicy, LaneGuard, TaskHandle, TaskKind,
            TaskPhase, TaskScope, TaskSnapshot, TaskStartResult,
        },
    },
    concurrency::resources::{Access, HolderInfo, PathScope, ResourceRequest},
    imageops::optimize::OptimizeOptions,
    library::{
        db::LibraryDb,
        folders as folder_service,
        history::{self, HistoryKind, NewOperation},
        inspect::{self as inspector, InspectScope, InspectionMode, InspectionReport},
        maintenance_actions, metadata,
        optimize_estimate::{self, OptimizeEstimate},
        pack_gc::{self, CompactMark, PackStats},
        paths::subtree_like_pattern,
        query::{self, PendingFilter},
        thumbs,
    },
};

use super::{AppState, MAX_CONSECUTIVE_FAILURES, MAX_INSPECTION_REPORTS};

impl AppState {
    pub fn cancel_task(&self, task_id: &str) -> AppResult<()> {
        self.tasks.cancel(task_id)
    }

    /// 临时区特征生成：只接受已校验的临时图片 id，占用 AI 服务资源，不写操作记录。
    pub fn start_temp_features(
        &self,
        kind: crate::ai::job::AiJobKind,
        image_ids: Vec<i64>,
    ) -> AppResult<TaskStartResult> {
        let settings = self.settings.snapshot().ai;
        self.ai.start_temp_features(
            self.app.clone(),
            &self.tasks,
            &settings,
            kind,
            image_ids,
            ConflictPolicy::Queue,
        )
    }

    /// 统一批量任务入口：缩略图重建、三种 AI 任务、全库相似分组。
    pub fn start_batch_task(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        match request.kind {
            BatchTaskKind::Thumbnail => self.start_thumbnail_task(app, request),
            BatchTaskKind::AiEmbedding | BatchTaskKind::AiPose | BatchTaskKind::AiDescription => {
                let kind = match request.kind {
                    BatchTaskKind::AiEmbedding => AiJobKind::Embedding,
                    BatchTaskKind::AiPose => AiJobKind::Pose,
                    _ => AiJobKind::Description,
                };
                let ai_request = AiJobRequest {
                    kind,
                    image_ids: Vec::new(),
                    overwrite: request.overwrite,
                    prompt: if matches!(kind, AiJobKind::Description) {
                        request.prompt.clone()
                    } else {
                        None
                    },
                    max_tokens: if matches!(kind, AiJobKind::Description) {
                        request.max_tokens
                    } else {
                        None
                    },
                };
                let settings = self.settings.snapshot().ai;
                self.ai.start_job(
                    app,
                    &self.tasks,
                    &settings,
                    ai_request,
                    request.scope,
                    request.conflict,
                )
            }
            BatchTaskKind::SimilarityGroups => self.ai.start_groups_task(
                app,
                &self.tasks,
                GroupOptions {
                    phash: request.group_phash,
                    phash_threshold: request.phash_threshold,
                    embedding: request.group_embedding,
                    embedding_threshold: request.embedding_threshold,
                },
                request.scope,
                request.conflict,
            ),
            BatchTaskKind::OptimizeEstimate => self.start_optimize_estimate(app, request),
            BatchTaskKind::Optimize => self.start_optimize_task(app, request),
            BatchTaskKind::CheckFast | BatchTaskKind::CheckFull => {
                self.start_check_task(app, request)
            }
            BatchTaskKind::Export => self.start_export_task(app, request),
        }
    }

    fn start_thumbnail_task(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        let (database, thumbs, root) = self.edit_handles()?;
        let ids = request.scope.resolve(&database)?;
        let only_missing = !request.overwrite;
        let recalc_visuals = request.recalc_visuals;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::Thumbnail,
            request.scope.label(),
        );
        let tasks = self.tasks.clone();
        let worker = handle.clone();
        let resources = self.resources();
        let images = self.images();
        self.tasks.launch(
            app,
            handle,
            // 缩略图重建会读旧原图再更新缩略图指针，必须避开编辑/压缩等内容写者。
            ResourceRequest::session_read()
                .with_content_writer()
                .with_path_note("正在读取"),
            request.conflict,
            move |guard| {
                let _ =
                    tasks.spawn_worker("thumbnail-rebuild".to_owned(), worker.clone(), move || {
                        // 内容写者由这个线程持有，直到所有图片处理结束。
                        let _guard: LaneGuard = guard;
                        worker.set_phase(TaskPhase::Preparing);
                        worker.set_message("正在统计待处理图片…");
                        let filter = if only_missing {
                            PendingFilter::Thumbnail
                        } else {
                            PendingFilter::All
                        };
                        let pending = match query::selected_pending_ids(&database, &ids, &filter) {
                            Ok(pending) => pending,
                            Err(error) => {
                                worker.set_message(error.to_string());
                                worker.finish(TaskPhase::Failed);
                                return;
                            }
                        };
                        worker.set_total(pending.len());
                        worker.set_phase(TaskPhase::Running);
                        worker.set_message("");
                        if pending.is_empty() {
                            worker.finish_from(&Ok(()));
                            return;
                        }
                        let holder =
                            HolderInfo::new(worker.id(), "重建缩略图").with_path_note("正在读取");
                        let mut consecutive_failures = 0_usize;
                        let mut settled = 0_usize;
                        let mut aborted = false;
                        for image_id in pending.iter().copied() {
                            if worker.should_cancel() {
                                break;
                            }
                            worker.set_current(Some(image_id));
                            match thumbs::rebuild(
                                &database,
                                &thumbs,
                                &root,
                                image_id,
                                only_missing,
                                recalc_visuals,
                                &resources,
                                &holder,
                                &|| worker.should_cancel(),
                                &images,
                            ) {
                                Ok(true) => {
                                    worker.inc_completed();
                                    consecutive_failures = 0;
                                    settled += 1;
                                }
                                Ok(false) => {
                                    worker.inc_skipped();
                                    consecutive_failures = 0;
                                    settled += 1;
                                }
                                Err(error) => {
                                    worker.inc_failed();
                                    worker.add_failure(Some(image_id), "", error.to_string());
                                    settled += 1;
                                    consecutive_failures += 1;
                                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                                        aborted = true;
                                        break;
                                    }
                                }
                            }
                        }
                        if aborted {
                            // 停在这里不是“完成”：剩下的图一张都没做过，必须说清楚。
                            let remaining = pending.len().saturating_sub(settled);
                            worker.finish_partial(format!(
                                "连续 {MAX_CONSECUTIVE_FAILURES} 张失败，任务已停止；还有 {remaining} 张未处理"
                            ));
                        } else {
                            worker.finish_from(&Ok(()));
                        }
                    });
            },
        )
    }

    /// 空间优化估算：只读扫描与抽样编码，结果按任务 id 缓存在内存里。
    fn start_optimize_estimate(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        let options = request.optimize.clone().unwrap_or_default();
        options.validate()?;
        ensure_optimizable_scope(&request.scope)?;
        let sample_size = request.estimate_sample_size.unwrap_or(200);
        if !optimize_estimate::sample_size_valid(sample_size) {
            return Err(AppError::message(format!(
                "估算抽样必须是 {}-{} 之间、步长 {} 的整数",
                optimize_estimate::SAMPLE_MIN,
                optimize_estimate::SAMPLE_MAX,
                optimize_estimate::SAMPLE_STEP
            )));
        }
        let (database, _, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let ids = request.scope.resolve(&database)?;
        if ids.is_empty() {
            return Err(AppError::message("当前范围没有图片"));
        }
        let fingerprint = optimize_fingerprint(&request.scope, &options);
        let scope = request.scope.clone();
        let scope_label = scope.label();
        let library_id = self
            .status()
            .ok()
            .flatten()
            .map(|info| info.id)
            .unwrap_or_default();
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::OptimizeEstimate,
            scope_label.clone(),
        );
        let task_id = handle.id().to_owned();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let estimates = self.optimize_estimates.clone();
        let resources = self.resources();
        let images = self.images();
        let memory = self.memory();
        self.tasks.launch(
            app,
            handle,
            ResourceRequest::session_read(),
            request.conflict,
            move |guard| {
                let _ =
                    tasks.spawn_worker("optimize-estimate".to_owned(), worker.clone(), move || {
                        let _guard: LaneGuard = guard;
                        worker.set_phase(TaskPhase::Running);
                        let holder = HolderInfo::new(task_id.clone(), "估算压缩空间")
                            .with_path_note("正在读取");
                        let result = optimize_estimate::estimate(
                            &database,
                            &root,
                            &task_id,
                            &ids,
                            &options,
                            sample_size,
                            &fingerprint,
                            &*worker,
                            &resources,
                            &images,
                            &memory,
                            &holder,
                        );
                        match result {
                            Ok(Some(mut estimate)) => {
                                // 结果要能独立重建执行请求：范围、参数与图库标识都在结果里。
                                estimate.scope = Some(scope);
                                estimate.scope_label = scope_label;
                                estimate.library_id = library_id;
                                store_estimate(&estimates, estimate);
                                worker.set_message("估算完成");
                                worker.finish_from(&Ok(()));
                            }
                            Ok(None) => worker.finish_from(&Ok(())),
                            Err(error) => {
                                worker.set_message(error.to_string());
                                worker.finish(TaskPhase::Failed);
                            }
                        }
                    });
            },
        )
    }

    /// 空间优化执行：先按估算凭证核对范围与参数，再拿到写资源后编码并发布。
    fn start_optimize_task(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        let options = request.optimize.clone().unwrap_or_default();
        options.validate()?;
        ensure_optimizable_scope(&request.scope)?;
        let estimate_id = request
            .estimate_id
            .as_deref()
            .ok_or_else(|| AppError::message("请先估算压缩空间，再确认执行"))?;
        let estimate = self
            .optimize_estimate(estimate_id)
            .ok_or_else(|| AppError::message("估算结果已过期，请重新估算"))?;

        let (database, thumbs, root) = self.edit_handles()?;
        let fingerprint = optimize_fingerprint(&request.scope, &options);
        let scope = request.scope.clone();
        let scope_label = scope.label();
        if scope.resolve(&database)?.is_empty() {
            return Err(AppError::message("当前范围没有图片"));
        }

        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::Optimize,
            scope_label,
        );
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let images = self.images();
        let memory = self.memory();
        // 压缩只替换图片内容、不改变文件夹结构与文件名，所以用“原地替换”而不是写锁：
        // 查看器在压缩期间仍能读原图，移动/删除/导入/另一个压缩照旧互斥。
        let resource_request = ResourceRequest::path_replace("")
            .with_timeline(Access::Read)
            .with_content_writer()
            .with_path_note("正在处理图库内容");
        self.tasks.launch(
            app,
            handle,
            resource_request,
            request.conflict,
            move |guard| {
                let _ = tasks.spawn_worker("optimize".to_owned(), worker.clone(), move || {
                    let _guard: LaneGuard = guard;
                    worker.set_phase(TaskPhase::Running);

                    // 复核段要逐张 stat 原图，几十万张时很久；给一句话说明在等什么。
                    worker.set_message("正在核对原图…");
                    let prepared = recheck_optimize_scope(
                        &database,
                        &root,
                        &scope,
                        &options,
                        &estimate,
                        &fingerprint,
                    );
                    let (candidates, count, dropped) = match prepared {
                        Ok(prepared) => prepared,
                        Err(error) => {
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                            return;
                        }
                    };
                    let operation = match history::LiveOperation::begin(
                        &database,
                        NewOperation::new(
                            HistoryKind::Optimize,
                            format!("优化图片空间 · {}", scope.label()),
                        ),
                    ) {
                        Ok(operation) => operation,
                        Err(error) => {
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                            return;
                        }
                    };
                    let operation_id = operation.id();
                    worker.set_operation_id(Some(operation_id));
                    worker.set_total(count as usize);
                    // 实际耗时从真正进入编码开始算，排除排队、复核与准备。
                    worker.mark_work_started();
                    worker.set_message("正在编码…");
                    let result = crate::library::optimize::optimize(
                        &database,
                        &root,
                        &thumbs,
                        candidates,
                        &options,
                        operation_id,
                        &*worker,
                        &images,
                        &memory,
                    );
                    // 已提交的批次会改变文件、大小与可选特征；成功或中途出错都要刷新缓存。
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    match result {
                        Ok(report) => {
                            let label = if report.processed > 0 {
                                format!(
                                    "优化图片空间 · {} 张 · 省 {}",
                                    report.processed,
                                    format_savings(report.savings_bytes)
                                )
                            } else {
                                "优化图片空间".to_owned()
                            };
                            let _ = history::update_label(&database, operation_id, &label);
                            let error = report.stop_reason.clone().unwrap_or_default();
                            if let Err(error) = operation.finish(&error) {
                                log::warn!("收尾空间优化记录失败：{error}");
                            }
                            worker.set_failed(report.failed as usize);
                            // 跳过只认候选池内的口径；执行前被排除的图片不算「已跳过」，
                            // 否则跑完那一刻会从 1.7 万跳到 7 万，环图与进度一起翻车。
                            worker.set_skipped(report.progress_skipped());
                            worker.set_skip_breakdown(&report.skip_buckets());
                            // 报告里的失败清单要进任务胶囊，否则用户只知道失败了几张。
                            for failure in &report.failures {
                                worker.add_failure(
                                    Some(failure.image_id),
                                    &failure.path,
                                    &failure.reason,
                                );
                            }
                            let cancelled = report.cancelled || worker.should_cancel();
                            let (phase, summary) = optimize_outcome(&report, dropped, cancelled);
                            worker.set_message(summary);
                            worker.finish(phase);
                        }
                        Err(error) => {
                            if let Err(error) = operation.finish(&error.to_string()) {
                                log::warn!("收尾空间优化记录失败：{error}");
                            }
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                        }
                    }
                });
            },
        )
    }

    pub fn optimize_estimate(&self, task_id: &str) -> Option<OptimizeEstimate> {
        self.optimize_estimates
            .lock()
            .iter()
            .find(|(id, _)| id == task_id)
            .map(|(_, estimate)| estimate.clone())
    }

    /// 批量任务开始前的只读预览；只取读连接，不建任务、不占资源表。
    pub fn preview_batch_task(
        &self,
        request: BatchTaskPreviewRequest,
    ) -> AppResult<BatchTaskPreview> {
        let (database, _, _) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        batch_preview::preview(&database, &request)
    }

    // ---------------- 维护：包回收、检查、孤儿、导出 ----------------

    /// 缩略图包当前的垃圾率；空闲循环用它做第 3 条判定，平时一次都不查。
    pub fn pack_garbage_ratio(&self) -> AppResult<f64> {
        let (database, _, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        Ok(pack_gc::stats(&database, &root)?.garbage_ratio())
    }

    pub fn pack_stats(&self) -> AppResult<PackStats> {
        let (database, _, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        pack_gc::stats(&database, &root)
    }

    pub fn last_pack_compact(&self) -> AppResult<Option<CompactMark>> {
        let (database, _) = self.query_source()?;
        pack_gc::last_compact(&database)
    }

    /// 手动「立即回收」：不等空闲条件，但仍受任务互斥（有导入在跑时会排队）。
    pub fn compact_pack_now(&self, app: AppHandle) -> AppResult<TaskStartResult> {
        self.start_pack_compact_inner(app, false)
    }

    /// 空闲循环提交的回收：用户一有活动就让位。
    pub fn start_pack_compact(&self, app: AppHandle) -> AppResult<TaskStartResult> {
        self.start_pack_compact_inner(app, true)
    }

    fn start_pack_compact_inner(
        &self,
        app: AppHandle,
        automatic: bool,
    ) -> AppResult<TaskStartResult> {
        let (database, thumbs, root) = self.edit_handles()?;
        let scope = if automatic {
            "空闲回收"
        } else {
            "立即回收"
        };
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::PackCompact,
            scope,
        );
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        // 与导入、编辑、重建缩略图互斥——这些正是唯一会 append 的路径；
        // `Access::Replace` 不与读互斥，查看器与 MCP 拼图照常可用。
        let request = ResourceRequest::path_replace("")
            .with_timeline(Access::Read)
            .with_content_writer()
            .with_path_note("正在回收缩略图包");
        self.tasks.launch(
            app,
            handle,
            request,
            ConflictPolicy::Queue,
            move |guard| {
                let _ = tasks.spawn_worker("pack-compact".to_owned(), worker.clone(), move || {
                    let _guard: LaneGuard = guard;
                    worker.set_phase(TaskPhase::Preparing);
                    worker.set_message("正在检查缩略图包…");
                    let user_back = move || {
                        if !automatic {
                            return false;
                        }
                        matches!(activity::idle_for(), Some(idle) if idle < idle_loop::idle_threshold())
                    };
                    match pack_gc::compact(&database, &root, &thumbs, &*worker, &user_back) {
                        Ok(report) => {
                            let (phase, message) = pack_compact_outcome(&report);
                            worker.set_message(message);
                            worker.finish(phase);
                        }
                        Err(error) => {
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                        }
                    }
                });
            },
        )
    }

    /// 检查报告按任务 id 取；被新的检查挤掉时返回 `None`。
    pub fn inspection_report(&self, task_id: &str) -> Option<InspectionReport> {
        self.inspection_reports
            .lock()
            .iter()
            .find(|(id, _)| id == task_id)
            .map(|(_, report)| report.clone())
    }

    /// 报告导出 CSV：后端直接写文件，几万行不塞进 IPC 再塞回来。
    ///
    /// `group` 为空时四组全写；给了 `missing / orphan / sizeMismatch / hashMismatch`
    /// 就只写那一组，界面上的「导出这一组」走这条。
    pub fn save_inspection_report(
        &self,
        task_id: &str,
        path: &Path,
        group: Option<&str>,
    ) -> AppResult<()> {
        const GROUPS: [&str; 4] = ["missing", "orphan", "sizeMismatch", "hashMismatch"];
        if let Some(group) = group
            && !GROUPS.contains(&group)
        {
            return Err(AppError::message("报告分组无效"));
        }
        let report = self
            .inspection_report(task_id)
            .ok_or_else(|| AppError::message("报告已被新的检查覆盖，请重新扫描"))?;
        let path = path.to_path_buf();
        if path.as_os_str().is_empty() {
            return Err(AppError::message("请选择保存位置"));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, inspector::report_csv_group(&report, group))?;
        Ok(())
    }

    fn start_check_task(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        let (database, _, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let mode = if request.kind == BatchTaskKind::CheckFull {
            InspectionMode::Full
        } else {
            InspectionMode::Fast
        };
        let folder_ids = check_scope_folder_ids(&database, &request.scope)?;
        let library_id = self.status()?.map(|info| info.id).unwrap_or_default();
        let label = format!("{} · {}", mode.label(), request.scope.label());
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::CheckLibrary,
            label,
        );
        let task_id = handle.id().to_owned();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let reports = self.inspection_reports.clone();
        // 不持路径锁：全库完整校验要读二三十分钟，长期占根路径读锁会挡住导入与编辑。
        self.tasks.launch(
            app,
            handle,
            ResourceRequest::session_read(),
            request.conflict,
            move |guard| {
                let _ = tasks.spawn_worker("library-check".to_owned(), worker.clone(), move || {
                    let _guard: LaneGuard = guard;
                    worker.set_phase(TaskPhase::Running);
                    let scope = InspectScope { folder_ids };
                    match inspector::inspect(
                        &database,
                        &root,
                        &task_id,
                        &library_id,
                        mode,
                        &scope,
                        &*worker,
                    ) {
                        Ok(report) => {
                            worker.set_message(inspection_summary(&report));
                            store_inspection_report(&reports, report);
                            worker.finish_from(&Ok::<(), AppError>(()));
                        }
                        Err(error) => {
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                        }
                    }
                });
            },
        )
    }

    fn start_export_task(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        let (database, _, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let target = request.target_dir.clone().unwrap_or_default();
        if target.trim().is_empty() {
            return Err(AppError::message("请选择导出目录"));
        }
        let target = PathBuf::from(target.trim());
        if !target.is_absolute() {
            return Err(AppError::message("导出目录必须是绝对路径"));
        }
        let ids = request.scope.resolve(&database)?;
        if ids.is_empty() {
            return Err(AppError::message("当前范围没有图片"));
        }
        let include_manifest = request.include_manifest;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::Export,
            format!("{} 张 · {}", ids.len(), request.scope.label()),
        );
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        // 根路径读锁 + 内容/元数据写者：导出期间导入、编辑、移动、删除都被拒绝。
        let resource = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_content_writer()
            .with_metadata_writer()
            .with_paths(vec![PathScope::read("")])
            .with_path_note("正在导出");
        self.tasks
            .launch(app, handle, resource, request.conflict, move |guard| {
                let _ = tasks.spawn_worker("export".to_owned(), worker.clone(), move || {
                    let _guard: LaneGuard = guard;
                    worker.set_phase(TaskPhase::Running);
                    match maintenance_actions::export(
                        &database,
                        &root,
                        &ids,
                        &target,
                        include_manifest,
                        &*worker,
                    ) {
                        Ok(report) => {
                            let mut message = format!(
                                "已导出 {} 张 · {}",
                                report.copied,
                                format_savings(report.bytes)
                            );
                            if report.skipped > 0 {
                                message.push_str(&format!(
                                    " · 跳过 {} 张（目标已存在）",
                                    report.skipped
                                ));
                            }
                            if !report.failed.is_empty() {
                                message.push_str(&format!(" · 失败 {} 张", report.failed.len()));
                                for failure in &report.failed {
                                    worker.add_failure(None, "", failure);
                                }
                            }
                            worker.set_message(&message);
                            worker.finish_from(&Ok::<(), AppError>(()));
                        }
                        Err(error) => {
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                        }
                    }
                });
            })
    }

    /// 孤儿移入回收站：必须基于一份当次扫描的报告，不接受任意路径列表。
    pub fn move_orphans_task(
        &self,
        app: AppHandle,
        report_task_id: &str,
        paths: Vec<String>,
    ) -> AppResult<TaskStartResult> {
        let report = self
            .inspection_report(report_task_id)
            .ok_or_else(|| AppError::message("报告已被新的检查覆盖，请先重新扫描"))?;
        let library_id = self
            .status()?
            .ok_or_else(|| AppError::message("尚未打开图库"))?
            .id;
        if report.library_id != library_id {
            return Err(AppError::message("这份报告属于另一个图库，请重新扫描"));
        }
        if paths.is_empty() {
            return Err(AppError::message("没有勾选要移走的孤儿文件"));
        }
        let (database, _, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::OrphanMove,
            format!("{} 个", paths.len()),
        );
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let reports = self.inspection_reports.clone();
        let report_task_id = report_task_id.to_owned();
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_paths(vec![PathScope::replace("")])
            .with_path_note("正在移动文件");
        self.tasks
            .launch(app, handle, request, ConflictPolicy::Queue, move |guard| {
                let _ = tasks.spawn_worker("orphan-move".to_owned(), worker.clone(), move || {
                    let _guard: LaneGuard = guard;
                    worker.set_phase(TaskPhase::Running);
                    match maintenance_actions::move_orphans(
                        &database, &root, &report, &paths, &*worker,
                    ) {
                        Ok(result) => {
                            let mut message = format!("已移入回收站 {} 个", result.moved);
                            if !result.failed.is_empty() {
                                message.push_str(&format!(" · 失败 {} 个", result.failed.len()));
                                for failure in &result.failed {
                                    worker.add_failure(
                                        None,
                                        "",
                                        format!("{}：{}", failure.path, failure.reason),
                                    );
                                }
                            }
                            worker.set_operation_id(result.operation_id);
                            worker.set_total(paths.len());
                            worker.set_completed(result.moved as usize);
                            worker.set_message(&message);
                            // 只摘真正搬走的条目：取消时没轮到的那部分仍留在报告里等下次。
                            prune_orphans(&reports, &report_task_id, &result.moved_paths);
                            worker.finish_from(&Ok::<(), AppError>(()));
                        }
                        Err(error) => {
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                        }
                    }
                });
            })
    }

    pub fn delete_zero_tags(&self) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::delete_zero_tags(&database))
    }

    pub fn tasks_snapshot(&self) -> Vec<TaskSnapshot> {
        self.tasks.list()
    }

    pub fn similarity_groups_result(&self, task_id: &str) -> Option<SimilarityGroupsResult> {
        self.ai.groups_result(task_id)
    }
}

/// 压缩执行的终态与摘要：失败张数、未删旧原图、未纳入候选都写进同一句话。
///
/// 用户取消优先于其他结果：取消时跑的仍是中途停止，不能落成「完成」。
pub(super) fn optimize_outcome(
    report: &crate::library::optimize::OptimizeReport,
    dropped: u64,
    cancelled: bool,
) -> (TaskPhase, String) {
    let mut summary = format!(
        "已替换 {} 张 · 省 {}",
        report.processed,
        format_savings(report.savings_bytes)
    );
    let skipped = report.progress_skipped();
    if skipped > 0 {
        summary.push_str(&format!(" · 已跳过 {skipped} 张"));
    }
    // 未纳入候选 = 前置判据排除 + 执行前复核剔除，与估算视图同名分组。
    let excluded = report.prefilter_skipped + dropped;
    if excluded > 0 {
        summary.push_str(&format!(" · 未纳入候选 {excluded} 张"));
    }
    if report.failed > 0 {
        summary.push_str(&format!(" · 失败 {} 张", report.failed));
    }
    // 旧原图删不掉时“省 X”并不成立：文件还在磁盘上，必须说出来。
    if report.delete_failed > 0 {
        summary.push_str(&format!(" · {} 张旧原图未删除", report.delete_failed));
    }
    let phase = if cancelled {
        TaskPhase::Cancelled
    } else if report.stop_reason.is_some() {
        TaskPhase::Failed
    } else if report.delete_failed > 0 {
        TaskPhase::PartialFailed
    } else {
        TaskPhase::Completed
    };
    (phase, summary)
}

/// 缩略图包回收的终态与文案：删不掉旧段不等于回收成功，半途出错也不抹掉已回收的段。
pub(super) fn pack_compact_outcome(report: &pack_gc::CompactReport) -> (TaskPhase, String) {
    use pack_gc::CompactStop;

    let freed = format_savings(report.freed_bytes);
    let mut message = match report.stop {
        CompactStop::Done => format!("已回收 {} 个段 · 释放 {freed}", report.segments),
        CompactStop::Yielded => format!(
            "已让位 · 已回收 {} 个段 · 释放 {freed}，剩余部分下次空闲继续",
            report.segments
        ),
        CompactStop::Cancelled => {
            format!("已停止 · 已回收 {} 个段 · 释放 {freed}", report.segments)
        }
        CompactStop::Failed => format!(
            "已回收 {} 个段 · 释放 {freed}，之后出错：{}",
            report.segments,
            report.error.as_deref().unwrap_or("未知错误")
        ),
    };
    if report.delete_failed > 0 {
        // 段行已经删了，残留文件不会再被回收任务碰到；下次打开图库时统一清掉。
        message.push_str(&format!(
            " · {} 个旧段删除失败，空间要等下次打开图库时清理",
            report.delete_failed
        ));
    }
    let phase = match report.stop {
        CompactStop::Yielded => TaskPhase::Yielded,
        CompactStop::Cancelled => TaskPhase::Cancelled,
        CompactStop::Failed if report.segments == 0 => TaskPhase::Failed,
        CompactStop::Failed => TaskPhase::PartialFailed,
        CompactStop::Done if report.delete_failed > 0 => TaskPhase::PartialFailed,
        CompactStop::Done => TaskPhase::Completed,
    };
    (phase, message)
}

fn store_estimate(store: &Mutex<VecDeque<(String, OptimizeEstimate)>>, estimate: OptimizeEstimate) {
    const MAX_ESTIMATES: usize = 10;
    let mut entries = store.lock();
    entries.push_front((estimate.task_id.clone(), estimate));
    entries.truncate(MAX_ESTIMATES);
}

/// 估算与执行共用同一份参数指纹；批次 2 的执行命令据此校验估算凭证。
/// 临时文件夹的图片在关闭图库时整目录清空，压缩它们只会留下一条无用记录，
/// 所以估算与执行都不接受它作为范围。
fn ensure_optimizable_scope(scope: &TaskScope) -> AppResult<()> {
    if matches!(scope, TaskScope::Temp) {
        return Err(AppError::message("临时文件夹不参与图片空间优化"));
    }
    Ok(())
}

fn optimize_fingerprint(scope: &crate::app::tasks::TaskScope, options: &OptimizeOptions) -> String {
    let options = serde_json::to_string(options).unwrap_or_default();
    blake3::hash(format!("{scope:?}|{options}").as_bytes())
        .to_hex()
        .to_string()
}

/// 排队期间范围、文件与估算凭证都可能变化，真正开始执行前重新收集与核对。
///
/// 三道关：参数指纹与张数/体积、候选集合指纹（都在 `verify_grant` 里）、逐张
/// `stat` 原图与目录存在性。任一环节与提交估算时不一致就返回错误，由调用方把任务
/// 标成失败，不使用过期候选。返回 `(候选, 可处理张数, 被剔除的张数)`。
fn recheck_optimize_scope(
    database: &LibraryDb,
    root: &std::path::Path,
    scope: &TaskScope,
    options: &OptimizeOptions,
    estimate: &OptimizeEstimate,
    fingerprint: &str,
) -> AppResult<(Vec<crate::library::optimize::CandidateRow>, u64, u64)> {
    let ids = scope.resolve(database)?;
    if ids.is_empty() {
        return Err(AppError::message("当前范围没有图片"));
    }
    let mut candidates = crate::library::optimize::collect_candidates(database, &ids)?;
    let (count, bytes) = crate::library::optimize::eligible_summary(&candidates, options);
    let set = crate::library::optimize::candidate_fingerprint(&candidates, options);
    crate::library::optimize::verify_grant(estimate, fingerprint, count, bytes, &set)?;

    // 目录被删时每个文件都会“缺失”，先给出更可操作的原因。
    let check = crate::library::optimize::verify_sources(root, &candidates, options);
    if let Some(folder) = check.missing_dirs.first() {
        return Err(AppError::message(format!(
            "有 {} 个文件夹已不存在（例如「{}」），请重新估算",
            check.missing_dirs.len(),
            if folder.is_empty() {
                "根目录"
            } else {
                folder
            }
        )));
    }
    let drifted = check.drifted();
    if drifted == 0 {
        return Ok((candidates, count, 0));
    }
    let ratio = drifted as f64 / count.max(1) as f64;
    if ratio > crate::library::optimize::SOURCE_DRIFT_LIMIT {
        return Err(AppError::message(format!(
            "有 {drifted} 张原图缺失或已变化（占 {:.1}%），请重新估算",
            ratio * 100.0
        )));
    }
    // 少量漂移：剔除这几张继续跑，任务摘要里说明。
    let bad: std::collections::HashSet<i64> = check.bad_ids.iter().copied().collect();
    candidates.retain(|row| !bad.contains(&row.id));
    let (remaining, _) = crate::library::optimize::eligible_summary(&candidates, options);
    Ok((candidates, remaining, drifted))
}

fn format_savings(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes / MB)
    } else {
        format!("{:.0} KB", bytes / KB)
    }
}

/// 检查范围落成「要遍历哪些文件夹」。
///
/// 收集按文件夹走，不看单张 id：遍历是按目录 `read_dir` 的，把范围收成文件夹集合
/// 才能保证内存是 O(最大单文件夹) 而不是 O(全库)。
fn check_scope_folder_ids(database: &LibraryDb, scope: &TaskScope) -> AppResult<Option<Vec<i64>>> {
    match scope {
        TaskScope::Library => Ok(None),
        TaskScope::Temp => Err(AppError::message("临时文件夹不参与检查")),
        TaskScope::Folder {
            folder_id,
            include_subfolders,
        } => {
            if *folder_id <= 0 {
                return Err(AppError::message("文件夹 id 无效"));
            }
            if !*include_subfolders {
                return Ok(Some(vec![*folder_id]));
            }
            let path = folder_service::info(database, *folder_id)?.path;
            let ids = database.with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT id FROM folders
                         WHERE (id = ?1 OR path LIKE ?2 ESCAPE '\\')
                           AND trashed_at IS NULL AND purge_operation_id IS NULL",
                )?;
                let rows = statement
                    .query_map(
                        rusqlite::params![folder_id, subtree_like_pattern(&path)],
                        |row| row.get::<_, i64>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })?;
            Ok(Some(ids))
        }
        other => {
            let ids = other.resolve(database)?;
            if ids.is_empty() {
                return Err(AppError::message("当前范围没有图片"));
            }
            let mut folders = std::collections::HashSet::new();
            for chunk in ids.chunks(400) {
                let marks = crate::library::placeholders(chunk.len());
                database.with_reader(|connection| {
                    let mut statement = connection.prepare(&format!(
                        "SELECT DISTINCT folder_id FROM images WHERE id IN ({marks})"
                    ))?;
                    for folder in statement
                        .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                            row.get::<_, i64>(0)
                        })?
                    {
                        folders.insert(folder?);
                    }
                    Ok(())
                })?;
            }
            if folders.is_empty() {
                return Err(AppError::message("当前范围没有可检查的文件夹"));
            }
            Ok(Some(folders.into_iter().collect()))
        }
    }
}

fn store_inspection_report(
    store: &Mutex<VecDeque<(String, InspectionReport)>>,
    report: InspectionReport,
) {
    let mut entries = store.lock();
    entries.push_front((report.task_id.clone(), report));
    entries.truncate(MAX_INSPECTION_REPORTS);
}

/// 移走之后把这几条从缓存报告里摘掉，界面不必重新扫描就能看到剩余项。
fn prune_orphans(
    store: &Mutex<VecDeque<(String, InspectionReport)>>,
    task_id: &str,
    moved: &[String],
) {
    let mut entries = store.lock();
    let Some((_, report)) = entries.iter_mut().find(|(id, _)| id == task_id) else {
        return;
    };
    let moved: std::collections::HashSet<&str> = moved.iter().map(String::as_str).collect();
    report
        .orphans
        .retain(|orphan| !moved.contains(orphan.path.as_str()));
    report.orphan_total = report.orphan_total.saturating_sub(moved.len() as u64);
    // `truncated` 是四份清单共用的标记，摘掉孤儿不能替别的清单消掉它，保持原样。
}

fn inspection_summary(report: &InspectionReport) -> String {
    let mut message = format!(
        "扫描 {} 张 · 缺失 {} · 孤儿 {} · 大小不符 {}",
        report.scanned_rows, report.missing_total, report.orphan_total, report.size_mismatch_total
    );
    if report.mode == InspectionMode::Full {
        message.push_str(&format!(" · 哈希不符 {}", report.hash_mismatch_total));
    }
    if report.changed_during_scan > 0 {
        message.push_str(&format!(" · 扫描期间变动 {}", report.changed_during_scan));
    }
    if report.is_clean() {
        message = format!("扫描 {} 张 · 没有发现问题", report.scanned_rows);
    }
    message
}

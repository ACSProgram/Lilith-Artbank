use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::{
    ai::{
        job::{AiJobKind, AiJobRequest},
        runtime::{AiRuntime, GroupOptions, SimilarityGroupsResult},
    },
    app::{
        activity,
        batch_preview::{self, BatchTaskPreview, BatchTaskPreviewRequest},
        error::{AppError, AppResult},
        idle::{self as idle_loop, IdleLoop},
        settings::SettingsStore,
        tasks::{
            BatchTaskKind, BatchTaskRequest, ConflictPolicy, LaneGuard, TaskBus, TaskHandle,
            TaskKind, TaskPhase, TaskScope, TaskSnapshot, TaskStartResult, TaskSummary,
        },
    },
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget},
        resources::{
            Access, Conflict, HolderInfo, PathScope, ResourceGuard, ResourceRequest, WaitError,
        },
    },
    imageops::optimize::OptimizeOptions,
    import::{
        ImportRequest, ImportSource, ImportTarget,
        job::{ImportJob, ImportSnapshot},
    },
    library::{
        BatchReporter,
        capability::{self, FeatureSummary, ImageCapabilities},
        collections::{self, CollectionInfo, CombineMode},
        content,
        db::LibraryDb,
        folders as folder_service,
        history::{self, HistoryKind, HistoryPage, NewOperation, RecoveryPlan},
        inspect::{self as inspector, InspectScope, InspectionMode, InspectionReport},
        maintenance_actions,
        manifest::{LibraryInspection, inspect},
        metadata::{
            self, DeleteReport, ImageDetails, ImageState, MoveReport, SelectionTags, TagRef,
        },
        optimize_estimate::{self, OptimizeEstimate},
        pack_gc::{self, CompactMark, PackStats},
        paths::{
            join_folder, normalize_folder, normalize_folder_name, parent_path, subtree_like_pattern,
        },
        query::{self, FolderChange, FolderInfo, ImagePage, LibraryQuery, PendingFilter, TagInfo},
        service::{self, LibraryCounts, LibraryInfo, LibrarySession},
        stats::{self, StatsRequest, StatsSnapshot},
        temp,
        thumbs::{self, ThumbPack},
        viewer::PoseData,
    },
    mcp::{
        approval::{ApprovalQueue, ApprovalView},
        context::ReadContext,
        runtime::McpRuntime,
        sheet::SheetCache,
    },
};

/// MCP 同步写入成功后通知前端刷新；载荷包含来源、操作记录 id 与是否需要关闭查看器。
pub const LIBRARY_CHANGED_EVENT: &str = "library-changed";

pub struct AppState {
    pub settings: SettingsStore,
    pub ai: AiRuntime,
    pub tasks: TaskBus,
    pub mcp: McpRuntime,
    app: AppHandle,
    /// 导入解码内存额度；按估算字节排队，避免大图并发撑高峰值。
    import_memory: Arc<ImageMemoryBudget>,
    library: Mutex<Option<LibrarySession>>,
    /// 最近发起的导入；允许排队后可能同时存在多个未结束的导入。
    import_jobs: Mutex<VecDeque<Arc<ImportJob>>>,
    /// 已结束的导入回执；MCP 任务列表在导入结束后仍要能读到统计与预览。
    import_receipts: Mutex<VecDeque<ImportSnapshot>>,
    managed: AtomicBool,
    recovery: Mutex<RecoveryPlan>,
    optimize_estimates: Arc<Mutex<VecDeque<(String, OptimizeEstimate)>>>,
    /// 最近几次检查的报告，按任务 id 索引；被挤掉时界面提示重新扫描。
    inspection_reports: Arc<Mutex<VecDeque<(String, InspectionReport)>>>,
    /// 空闲回收线程；开库时启动，关库时停。
    idle_loop: Mutex<Option<IdleLoop>>,
    mcp_sheets: Arc<Mutex<SheetCache>>,
    mcp_write: Arc<Mutex<()>>,
    mcp_approvals: Arc<ApprovalQueue>,
    mcp_tasks: Arc<Mutex<VecDeque<String>>>,
}

/// MCP 能取消的任务只保留最近这些条，足够覆盖一次确认后的等待窗口。
const MAX_MCP_TASKS: usize = 100;
/// 导入回执与未结束导入的句柄保留同样量级，旧的滚动淘汰；运行中的导入永不淘汰。
const MAX_IMPORT_RECEIPTS: usize = 20;
/// 检查报告缓存条数；与压缩估算的 10 条同一个滚动淘汰模式。
const MAX_INSPECTION_REPORTS: usize = 5;

/// 缩略图重建连续失败达到该数量即停止，避免把整库都记成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;

impl AppState {
    pub fn new(settings: SettingsStore, app: AppHandle) -> Self {
        let mcp_approvals = Arc::new(ApprovalQueue::new(app.clone()));
        let images = Arc::new(ImageBudget::from_setting(
            settings.snapshot().performance.worker_threads,
        ));
        let tasks = TaskBus::with_images(images.clone());
        let ai = AiRuntime::with_runtime(tasks.resources(), images);
        Self {
            settings,
            ai,
            tasks,
            mcp: McpRuntime::new(),
            app,
            import_memory: Arc::new(ImageMemoryBudget::default()),
            library: Mutex::new(None),
            import_jobs: Mutex::new(VecDeque::new()),
            import_receipts: Mutex::new(VecDeque::new()),
            managed: AtomicBool::new(false),
            recovery: Mutex::new(RecoveryPlan::default()),
            optimize_estimates: Arc::new(Mutex::new(VecDeque::new())),
            inspection_reports: Arc::new(Mutex::new(VecDeque::new())),
            idle_loop: Mutex::new(None),
            mcp_sheets: Arc::new(Mutex::new(SheetCache::default())),
            mcp_write: Arc::new(Mutex::new(())),
            mcp_approvals,
            mcp_tasks: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn current_thumb_source(&self) -> Option<(Arc<LibraryDb>, Arc<ThumbPack>)> {
        self.library_handles()
            .map(|(database, thumbs, _)| (database, thumbs))
    }

    pub fn current_library(&self) -> Option<(Arc<LibraryDb>, PathBuf)> {
        self.library_handles()
            .map(|(database, _, root)| (database, root))
    }

    /// 一次取锁拿全当前图库的三件句柄，避免同一操作里重复加锁。
    fn library_handles(&self) -> Option<(Arc<LibraryDb>, Arc<ThumbPack>, PathBuf)> {
        self.library.lock().as_ref().map(|session| {
            (
                session.db.clone(),
                session.thumbs.clone(),
                PathBuf::from(&session.info.path),
            )
        })
    }

    /// 返回缓存的状态；计数不再随每次状态查询重算，由 [`Self::counts`] 按需刷新。
    pub fn status(&self) -> AppResult<Option<LibraryInfo>> {
        Ok(self
            .library
            .lock()
            .as_ref()
            .map(|session| session.info.clone()))
    }

    /// 重算三档计数并写回缓存；前端在开库、数据变化后单独调用。
    pub fn counts(&self) -> AppResult<LibraryCounts> {
        let Some((database, library_id)) = self
            .library
            .lock()
            .as_ref()
            .map(|session| (session.db.clone(), session.info.id.clone()))
        else {
            return Err(AppError::message("尚未打开图库"));
        };
        let (image_count, trash_count, temp_count) = database.counts()?;
        let mut library = self.library.lock();
        if let Some(session) = library.as_mut()
            && session.info.id == library_id
        {
            session.info.image_count = image_count;
            session.info.trash_count = trash_count;
            session.info.temp_count = temp_count;
        }
        Ok(LibraryCounts {
            image_count,
            trash_count,
            temp_count,
        })
    }

    pub fn start_import(
        &self,
        app: AppHandle,
        request: ImportRequest,
    ) -> AppResult<ImportSnapshot> {
        self.start_import_inner(app, request, false)
            .map(|(snapshot, _)| snapshot)
    }

    /// MCP 导入：与界面导入同一条管线，固定不压缩，并记录 AI 前缀与可取消任务。
    pub fn start_mcp_import(
        &self,
        mut request: ImportRequest,
    ) -> AppResult<(ImportSnapshot, Option<i64>)> {
        request.optimize = None;
        request.worker_threads = None;
        self.start_import_inner(self.app.clone(), request, true)
    }

    fn start_import_inner(
        &self,
        app: AppHandle,
        request: ImportRequest,
        from_mcp: bool,
    ) -> AppResult<(ImportSnapshot, Option<i64>)> {
        let request = request.normalized()?;

        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;

        let temp_target = request.target == ImportTarget::Temp;
        let target_path = if temp_target {
            database
                .with_reader(crate::library::temp::path)?
                .ok_or_else(|| AppError::message("临时文件夹不存在"))?
        } else {
            request.folder.clone()
        };
        let resource_request = if temp_target {
            // 临时导入不写操作记录，也不占用时间线，撤销/重做无需等它。
            ResourceRequest::session_read()
                .with_content_writer()
                .with_paths(vec![PathScope::read(&target_path)])
                .with_path_note("正在导入到")
        } else {
            ResourceRequest::session_read()
                .with_timeline(Access::Read)
                .with_content_writer()
                .with_paths(vec![PathScope::read(&target_path)])
                .with_path_note("正在导入到")
        };
        let scope = if temp_target {
            format!("{} 个路径 · 临时", request.paths.len())
        } else {
            format!("{} 个路径", request.paths.len())
        };
        let job = Arc::new(ImportJob::new(
            uuid::Uuid::new_v4().to_string(),
            scope,
            request.target,
        ));
        let handle = job.handle();
        let runner_job = job.clone();
        let runner_app = app.clone();
        let label = if temp_target {
            format!("导入 {} 个路径到临时区", request.paths.len())
        } else if request.folder.is_empty() {
            format!("导入 {} 个路径", request.paths.len())
        } else {
            format!(
                "导入 {} 个路径到「{}」",
                request.paths.len(),
                request.folder
            )
        };
        let label = if from_mcp {
            format!("AI · {label}")
        } else {
            label
        };
        // 临时导入是生命周期数据：不写操作记录，也不影响撤销/重做时间线。
        let operation = if temp_target {
            None
        } else {
            Some(history::LiveOperation::begin(
                &database,
                NewOperation::new(HistoryKind::Import, label),
            )?)
        };
        let operation_id = operation.as_ref().map(history::LiveOperation::id);
        handle.set_operation_id(operation_id);
        let source = ImportSource {
            database: database.clone(),
            thumbs,
            root,
            operation_id,
            resources: self.resources(),
            images: self.images(),
            memory: self.import_memory(),
            holder: HolderInfo::new(job.id(), "导入")
                .with_reason("正在导入")
                .with_path_note("正在导入到"),
        };
        let started = self.tasks.launch(
            app,
            handle,
            resource_request,
            ConflictPolicy::Queue,
            move |guard| {
                crate::import::start(runner_app, source, runner_job, request, guard, operation);
            },
        )?;
        if let TaskStartResult::Conflict { active } = started {
            return Err(conflict_error(&active));
        }
        {
            let mut jobs = self.import_jobs.lock();
            jobs.push_back(job.clone());
            while jobs.len() > MAX_IMPORT_RECEIPTS {
                let Some(index) = jobs.iter().position(|job| !job.is_running()) else {
                    break;
                };
                jobs.remove(index);
            }
        }
        if from_mcp {
            self.remember_mcp_task(job.id());
        }
        Ok((job.snapshot(), operation_id))
    }

    /// 导入结束后由导入线程登记；任务列表按任务 id 取回统计与预览。
    pub fn remember_import_receipt(&self, snapshot: ImportSnapshot) {
        let mut receipts = self.import_receipts.lock();
        receipts.retain(|receipt| receipt.progress.job_id != snapshot.progress.job_id);
        receipts.push_front(snapshot);
        receipts.truncate(MAX_IMPORT_RECEIPTS);
    }

    /// 进行中的导入取实时快照，已结束的取回执。
    pub fn import_receipt(&self, job_id: &str) -> Option<ImportSnapshot> {
        self.import_status(job_id).or_else(|| {
            self.import_receipts
                .lock()
                .iter()
                .find(|receipt| receipt.progress.job_id == job_id)
                .cloned()
        })
    }

    pub fn cancel_import(&self, job_id: &str) -> AppResult<()> {
        let job = self
            .import_jobs
            .lock()
            .iter()
            .find(|job| job.id() == job_id)
            .cloned()
            .ok_or_else(|| AppError::message("导入任务不存在或已结束"))?;
        if !job.is_running() {
            return Err(AppError::message("导入任务已经结束"));
        }
        job.request_cancel();
        // 排队中的导入不会自己看到取消标记，还要从任务总线的等待队列里醒来。
        let _ = self.tasks.cancel(job.id());
        Ok(())
    }

    pub fn import_status(&self, job_id: &str) -> Option<ImportSnapshot> {
        self.import_jobs
            .lock()
            .iter()
            .find(|job| job.id() == job_id)
            .map(|job| job.snapshot())
    }

    pub(crate) fn resources(&self) -> Arc<crate::concurrency::resources::ResourceTable> {
        self.tasks.resources()
    }

    pub(crate) fn images(&self) -> Arc<ImageBudget> {
        self.tasks.images()
    }

    pub(crate) fn import_memory(&self) -> Arc<ImageMemoryBudget> {
        self.import_memory.clone()
    }

    pub(crate) fn move_task_request(
        &self,
        image_ids: &[i64],
        folder_path: &str,
    ) -> AppResult<ResourceRequest> {
        let (database, _) = self.query_source()?;
        move_resource_request(&database, image_ids, folder_path)
    }

    fn short_holder(label: &str, reason: impl Into<String>) -> HolderInfo {
        HolderInfo::new(uuid::Uuid::new_v4().to_string(), label).with_reason(reason)
    }

    fn wait_error(error: WaitError, action: &str) -> AppError {
        match error {
            WaitError::Cancelled => AppError::message("操作已取消"),
            WaitError::Timeout(conflicts) => AppError::message(format!(
                "{}，请等待完成或取消后再{action}",
                conflict_sentence(&conflicts)
            )),
        }
    }

    /// 文件夹结构改动用：会话读 + 时间线共享 + 路径写，最多等 10 秒。
    pub(crate) fn begin_folder_change(
        &self,
        label: &str,
        paths: Vec<PathScope>,
    ) -> AppResult<ResourceGuard> {
        let holder = Self::short_holder(label, "正在维护文件夹");
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_paths(paths);
        self.resources()
            .acquire(
                &request,
                &holder,
                Some(Instant::now() + Duration::from_secs(10)),
                &|| false,
            )
            .map_err(|error| Self::wait_error(error, "重试"))
    }

    /// 打开/创建/关闭图库：要求没有任务、短操作与排队任务。
    fn begin_session_write(
        &self,
        label: &str,
        action: &str,
        wait: Option<Duration>,
    ) -> AppResult<ResourceGuard> {
        let holder = Self::short_holder(label, "正在切换图库");
        let request = ResourceRequest::session_write();
        let table = self.resources();
        match wait {
            Some(wait) => table
                .acquire(&request, &holder, Some(Instant::now() + wait), &|| false)
                .map_err(|error| Self::wait_error(error, action)),
            None => table.try_acquire(&request, &holder).map_err(|conflicts| {
                AppError::message(format!(
                    "{}，请等待完成或取消后再{action}",
                    conflict_sentence(&conflicts)
                ))
            }),
        }
    }

    pub(crate) fn begin_metadata_write(&self, label: &str) -> AppResult<ResourceGuard> {
        let holder = Self::short_holder(label, "正在修改图片信息");
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_metadata_writer();
        self.tasks
            .acquire_immediate(&request, &holder)
            .map(|guard| guard.into_resource())
    }

    /// 编辑保存等短内容写：会话读 + 时间线共享 + 内容写 + 路径写，立即失败。
    pub(crate) fn begin_content_write(
        &self,
        label: &str,
        paths: Vec<PathScope>,
    ) -> AppResult<ResourceGuard> {
        let holder = Self::short_holder(label, "正在写入图库内容");
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_content_writer()
            .with_paths(paths);
        self.tasks
            .acquire_immediate(&request, &holder)
            .map(|guard| guard.into_resource())
    }

    /// `application.manage(state)` 之后调用：启动时打开的库要等状态注册完再安排恢复任务。
    pub fn after_manage(&self) {
        self.managed.store(true, Ordering::SeqCst);
        self.after_library_open();
        self.mcp
            .apply(self.app.clone(), &self.settings.snapshot().mcp);
    }

    /// 设置保存后按新的 MCP 配置重启服务；关闭时删除运行时文件。
    pub fn apply_mcp_settings(&self, settings: &crate::app::settings::McpSettings) {
        // 关闭服务或切到只读后，旧确认单已经不能执行，直接丢弃，避免留下点不动的卡片。
        if !settings.enabled || settings.mode == crate::app::settings::McpMode::ReadOnly {
            self.mcp_approvals.clear();
        }
        self.mcp.apply(self.app.clone(), settings);
    }

    pub fn mcp_status(&self) -> crate::mcp::runtime::McpStatus {
        self.mcp.status(&self.settings.snapshot().mcp)
    }

    /// 只读工具的一次快照：图库句柄、计数、设置与拼图映射缓存。
    pub(crate) fn mcp_context(&self) -> AppResult<ReadContext> {
        let info = self
            .status()?
            .ok_or_else(|| AppError::message("尚未打开图库，请先在 Lilith Artbank 中打开图库"))?;
        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库，请先在 Lilith Artbank 中打开图库"))?;
        Ok(ReadContext {
            database,
            thumbs,
            root,
            info,
            settings: self.settings.snapshot(),
            sheets: self.mcp_sheets.clone(),
            resources: self.resources(),
            images: self.images(),
        })
    }

    /// MCP 写入在本进程内串行；与任务总线无关，只保护多个客户端同时调用。
    pub(crate) fn lock_mcp_writes(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.mcp_write.lock()
    }

    /// MCP 同步写入后的统一收尾：失效页面缓存与相似结果，裁剪记录，再通知前端刷新。
    pub(crate) fn after_mcp_write(&self, payload: Value, invalidate_similarity: bool) {
        query::clear_page_cache();
        if invalidate_similarity {
            self.ai.invalidate_similarity();
        }
        self.after_history_write();
        let _ = self.app.emit(LIBRARY_CHANGED_EVENT, payload);
    }

    pub(crate) fn mcp_approvals(&self) -> Arc<ApprovalQueue> {
        self.mcp_approvals.clone()
    }

    /// 待确认与执行中的列表；绑定任务的状态按任务总线实时推导。
    pub fn mcp_approval_views(&self) -> Vec<ApprovalView> {
        use crate::mcp::approval::ApprovalStatus;
        let snapshots = self.tasks.list();
        self.mcp_approvals
            .list()
            .into_iter()
            .map(|mut view| {
                if view.status == ApprovalStatus::Running
                    && let Some(task_id) = view.task_id.as_deref()
                    && let Some(task) = snapshots
                        .iter()
                        .find(|snapshot| snapshot.task_id == task_id)
                {
                    match task.phase {
                        TaskPhase::Completed => view.status = ApprovalStatus::Completed,
                        TaskPhase::Failed => {
                            view.status = ApprovalStatus::Failed;
                            if view.error.is_none() {
                                view.error = Some(task.message.clone());
                            }
                        }
                        TaskPhase::Cancelled => {
                            view.status = ApprovalStatus::Failed;
                            view.error = Some("任务已取消".to_owned());
                        }
                        _ => {}
                    }
                }
                view
            })
            .collect()
    }

    pub fn reject_mcp_approval(&self, id: &str) -> AppResult<()> {
        self.mcp_approvals.reject(id)
    }

    /// 用户批准后执行；写操作重新解析 selector 并核对数量，变化时只返回错误，不落库。
    pub fn approve_mcp_approval(&self, id: &str) -> AppResult<Value> {
        let record = self.mcp_approvals.begin(id)?;
        let settings = self.settings.snapshot().mcp;
        if !settings.enabled {
            let error = AppError::message("MCP 服务已关闭，不能执行待确认操作");
            self.mcp_approvals.fail(id, error.to_string());
            return Err(error);
        }
        if settings.mode == crate::app::settings::McpMode::ReadOnly {
            let error = AppError::message("当前是只读模式，MCP 写入已关闭");
            self.mcp_approvals.fail(id, error.to_string());
            return Err(error);
        }
        let tool = record.action.tool.clone();
        match crate::mcp::tools::execute_approval(self, &record.action) {
            Ok(outcome) => {
                let operation = outcome
                    .value
                    .get("operationId")
                    .and_then(Value::as_i64)
                    .map_or_else(|| "-".to_owned(), |id| id.to_string());
                crate::mcp::audit::log_line(
                    "approval",
                    &format!(
                        "approval={id} tool={tool} status=approved operation={operation} task={}",
                        outcome.task_id.as_deref().unwrap_or("-")
                    ),
                );
                if let Some(task_id) = &outcome.task_id {
                    self.remember_mcp_task(task_id);
                    self.mcp_approvals
                        .attach_task(id, task_id, outcome.value.clone());
                } else if outcome.value["status"] == "rejected" {
                    // 执行前复核失败（数量或状态变化）不是成功，也不能让确认单看起来已完成。
                    let reason = outcome.value["reason"]
                        .as_str()
                        .unwrap_or("执行前核对未通过");
                    self.mcp_approvals
                        .fail_with(id, reason, Some(outcome.value.clone()));
                } else {
                    self.mcp_approvals.complete(id, outcome.value.clone());
                }
                Ok(outcome.value)
            }
            Err(error) => {
                crate::mcp::audit::log_line(
                    "approval",
                    &format!("approval={id} tool={tool} status=error error={error}"),
                );
                self.mcp_approvals.fail(id, error.to_string());
                Err(error)
            }
        }
    }

    pub(crate) fn remember_mcp_task(&self, task_id: &str) {
        let mut tasks = self.mcp_tasks.lock();
        if tasks.iter().any(|id| id == task_id) {
            return;
        }
        tasks.push_back(task_id.to_owned());
        while tasks.len() > MAX_MCP_TASKS {
            tasks.pop_front();
        }
    }

    pub(crate) fn is_mcp_task(&self, task_id: &str) -> bool {
        self.mcp_tasks.lock().iter().any(|id| id == task_id)
    }

    /// MCP 发起的移动：后台任务，完成后任务快照带 `operationId`。
    pub fn start_mcp_move_task(
        &self,
        image_ids: Vec<i64>,
        folder_path: String,
    ) -> AppResult<TaskStartResult> {
        let (database, _, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let scope = format!("{} 张", image_ids.len());
        let request = move_resource_request(&database, &image_ids, &folder_path)?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::FolderMove,
            scope,
        );
        let task_id = handle.id().to_owned();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let launched = self.tasks.launch(
            self.app.clone(),
            handle,
            request,
            ConflictPolicy::Queue,
            move |guard| {
                let database = database.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker("mcp-move".to_owned(), task.clone(), move || {
                    let _guard: LaneGuard = guard;
                    task.set_phase(TaskPhase::Running);
                    let result = metadata::move_images_with_id(
                        &database,
                        &root,
                        &image_ids,
                        &folder_path,
                        &*task,
                    );
                    match &result {
                        Ok((report, operation_id)) => {
                            if let Some(operation_id) = operation_id {
                                let _ = history::prefix_label(&database, *operation_id, "AI · ");
                            }
                            task.set_operation_id(*operation_id);
                            // 计数字段与 message 用同一份报告，别出现「跳过 1 张」但 skipped=0。
                            task.set_skipped(report.skipped as usize);
                            task.set_failed(report.failures.len());
                            for failure in &report.failures {
                                task.add_failure(failure.image_id, &failure.path, &failure.reason);
                            }
                            let mut summary =
                                format!("移动 {} 张，跳过 {} 张", report.moved, report.skipped);
                            if !report.failures.is_empty() {
                                summary.push_str(&format!("，失败 {} 张", report.failures.len()));
                            }
                            task.set_message(summary);
                        }
                        Err(error) => task.set_message(error.to_string()),
                    }
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    task.finish_from(&result.map(|_| ()));
                });
            },
        )?;
        if matches!(launched, TaskStartResult::Started { .. }) {
            self.remember_mcp_task(&task_id);
        }
        Ok(launched)
    }

    pub fn mcp_undo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let result = self.undo_to(target_id)?;
        if let TaskStartResult::Started { task } = &result {
            self.remember_mcp_task(&task.task_id);
        }
        Ok(result)
    }

    pub fn mcp_redo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let result = self.redo_to(target_id)?;
        if let TaskStartResult::Started { task } = &result {
            self.remember_mcp_task(&task.task_id);
        }
        Ok(result)
    }

    pub fn create_library(&self, path: &Path, name: &str) -> AppResult<LibraryInfo> {
        let _session = self.begin_session_write("创建图库", "创建图库", None)?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let (session, _manifest) = service::create(path, name, read_connections)?;
        let info = session.info.clone();
        self.settings.set_library_path(Some(info.path.clone()))?;
        self.clear_previous_temp();
        self.ai.library_opened(
            info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            PathBuf::from(&info.path),
        );
        // 刚开库算「人在」，别让空闲循环在用户还没碰过界面时先动手。
        activity::touch();
        *self.library.lock() = Some(session);
        *self.recovery.lock() = RecoveryPlan::default();
        self.optimize_estimates.lock().clear();
        self.inspection_reports.lock().clear();
        self.import_receipts.lock().clear();
        self.import_jobs.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        self.restart_idle_loop();
        Ok(info)
    }

    pub fn open_library(&self, path: &Path) -> AppResult<LibraryInfo> {
        let _session = self.begin_session_write("打开图库", "打开图库", None)?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let session = service::open(path, read_connections)?;
        let plan = history::recover_on_open(&session.db)?;
        if let Err(error) = history::cleanup_undo_dirs(&session.db, Path::new(&session.info.path)) {
            log::warn!("清理撤销暂存目录失败：{error}");
        }
        // 上次退出没能清掉的临时图片在这里补清；计数由前端随后单独拉取。
        if let Err(error) = temp::clear(&session.db, Path::new(&session.info.path)) {
            log::warn!("清空临时文件夹失败：{error}");
        }
        // 让位、提交失败与进程崩溃都会在 thumbs/ 留下没登记的段文件。
        self.cleanup_pack_residue(&session.db, Path::new(&session.info.path));
        let info = session.info.clone();
        self.settings.set_library_path(Some(info.path.clone()))?;
        self.clear_previous_temp();
        self.ai.library_opened(
            info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            PathBuf::from(&info.path),
        );
        // 刚开库算「人在」，别让空闲循环在用户还没碰过界面时先动手。
        activity::touch();
        *self.library.lock() = Some(session);
        *self.recovery.lock() = plan;
        self.optimize_estimates.lock().clear();
        self.inspection_reports.lock().clear();
        self.import_receipts.lock().clear();
        self.import_jobs.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        self.restart_idle_loop();
        drop(_session);
        if self.managed.load(Ordering::SeqCst) {
            self.after_library_open();
        }
        Ok(info)
    }

    /// 打开图库时清理缩略图包残留：`thumbs/` 里不在 `thumb_segments` 表里的 `.pack` 都删。
    ///
    /// 覆盖三种来源：让位时没删成的、提交失败留下的、进程崩溃留在磁盘上的。
    fn cleanup_pack_residue(&self, database: &LibraryDb, root: &Path) {
        match pack_gc::cleanup_orphan_segments(database, root) {
            Ok(0) => {}
            Ok(removed) => log::info!("清理了 {removed} 个残留缩略图包"),
            Err(error) => log::warn!("清理残留缩略图包失败：{error}"),
        }
    }

    /// 开库时起空闲回收线程；换库时先停旧的，避免两个循环同时提交任务。
    fn restart_idle_loop(&self) {
        // 两次取锁不嵌套：旧线程的 Drop 要 join，不能在持锁时等。
        *self.idle_loop.lock() = None;
        if self.library.lock().is_some() {
            let started = IdleLoop::start(self.app.clone());
            *self.idle_loop.lock() = Some(started);
        }
    }

    fn stop_idle_loop(&self) {
        *self.idle_loop.lock() = None;
    }

    /// 切换图库时清空旧库的临时区；失败只记日志，不能挡住换库。
    fn clear_previous_temp(&self) {
        let Some((database, _, root)) = self.library_handles() else {
            return;
        };
        if let Err(error) = temp::clear(&database, &root) {
            log::warn!("切换图库前清空临时文件夹失败：{error}");
        }
        self.ai.invalidate_similarity();
        self.mcp_sheets.lock().clear();
    }

    /// 启动时按设置自动打开上次的图库；失败只记录，不阻塞应用启动。
    pub fn open_configured_library(&self) {
        let Some(path) = self.settings.snapshot().library_path else {
            return;
        };
        if let Err(error) = self.open_library(Path::new(&path)) {
            log::warn!("上次打开的图库无法加载（{path}）：{error}");
        }
    }

    pub fn close_library(&self) -> AppResult<()> {
        // 内容写任务不能中途关库；排队中的写任务也算，不能等关库时再启动。
        if let Some(task) = self
            .tasks
            .active()
            .into_iter()
            .find(|task| !task.kind.can_cancel_for_close())
        {
            let state = if task.phase == TaskPhase::Queued {
                "正在排队"
            } else {
                "正在进行"
            };
            return Err(AppError::message(format!(
                "「{}」{state}，请等待完成或取消后再关闭图库",
                task.label
            )));
        }
        self.tasks.cancel_for_close();
        // 可取消任务与短操作释放资源后再关库；超时会点名占用者。
        let _session =
            self.begin_session_write("关闭图库", "关闭图库", Some(Duration::from_secs(10)))?;
        if let Some((database, _, root)) = self.library_handles()
            && let Err(error) = temp::clear(&database, &root)
        {
            log::warn!("关闭图库前清空临时文件夹失败：{error}");
        }
        self.ai.library_closed();
        self.stop_idle_loop();
        activity::reset();
        *self.library.lock() = None;
        *self.recovery.lock() = RecoveryPlan::default();
        self.optimize_estimates.lock().clear();
        self.inspection_reports.lock().clear();
        self.import_receipts.lock().clear();
        self.import_jobs.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        self.settings.set_library_path(None)?;
        Ok(())
    }

    /// 应用退出：没有写任务时尽量清空临时区；来不及清的下次打开图库会补清。
    pub fn shutdown(&self) {
        if self
            .tasks
            .active()
            .iter()
            .any(|task| !task.kind.can_cancel_for_close())
        {
            log::warn!("退出时仍有写任务，临时文件夹留到下次打开图库时清空");
            return;
        }
        // 排队任务还没占资源，退出前直接取消，避免关闭过程中突然启动。
        self.tasks.cancel_queued();
        self.tasks.cancel_for_close();
        self.stop_idle_loop();
        if !self.tasks.wait_idle(Duration::from_secs(2)) {
            log::warn!("退出时任务未停止，临时文件夹留到下次打开图库时清空");
            return;
        }
        let Ok(_session) = self.begin_session_write("退出", "退出", Some(Duration::from_secs(2)))
        else {
            log::warn!("退出时图库仍被占用，临时文件夹留到下次打开图库时清空");
            return;
        };
        if let Some((database, _, root)) = self.library_handles()
            && let Err(error) = temp::clear(&database, &root)
        {
            log::warn!("退出时清空临时文件夹失败：{error}");
        }
    }

    pub fn rename_current_library(&self, name: &str) -> AppResult<LibraryInfo> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 120 {
            return Err(AppError::message("图库名称必须为 1-120 个字符"));
        }
        let mut guard = self.library.lock();
        let session = guard
            .as_mut()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        service::rename_manifest(Path::new(&session.info.path), trimmed)?;
        session.info.name = trimmed.to_owned();
        Ok(session.info.clone())
    }

    pub fn inspect_library(&self, path: &Path) -> LibraryInspection {
        inspect(path)
    }

    fn query_source(&self) -> AppResult<(Arc<LibraryDb>, PathBuf)> {
        self.library_handles()
            .map(|(database, _, root)| (database, root))
            .ok_or_else(|| AppError::message("尚未打开图库"))
    }

    /// 会改变图片集合、排序字段或筛选字段的写入：成功后让 id 列表游标整体失效。
    ///
    /// 有序 id 列表按查询指纹缓存，任何一条这样的写入都可能让它过期；失败时数据库已回滚，
    /// 缓存继续有效，不必清空。
    fn mutate<T>(&self, operation: impl FnOnce() -> AppResult<T>) -> AppResult<T> {
        let result = operation();
        if result.is_ok() {
            query::clear_page_cache();
            self.after_history_write();
        }
        result
    }

    pub fn query_images(&self, query: LibraryQuery) -> AppResult<ImagePage> {
        let (database, root) = self.query_source()?;
        query::query_page(&database, &root, query)
    }

    pub fn query_image_ids(&self, query: LibraryQuery) -> AppResult<Vec<i64>> {
        let (database, _) = self.query_source()?;
        query::query_ids(&database, query)
    }

    pub fn library_stats(&self, request: StatsRequest) -> AppResult<StatsSnapshot> {
        let (database, _) = self.query_source()?;
        stats::library_stats(&database, request)
    }

    pub fn folders(&self) -> AppResult<Vec<FolderInfo>> {
        let (database, _) = self.query_source()?;
        query::list_folders(&database)
    }

    pub fn create_folder(&self, path: &str) -> AppResult<FolderChange> {
        let normalized = normalize_folder(path)?;
        // 父级一起锁：新建会更新同级顺序，两个新文件夹并发创建时不能读到同一个末尾序号。
        let parent = parent_path(&normalized);
        let _guard = self.begin_folder_change(
            "新建文件夹",
            vec![PathScope::write(parent), PathScope::write(&normalized)],
        )?;
        let (database, root) = self.query_source()?;
        self.mutate(|| folder_service::create(&database, &root, path))
            .map(folder_change)
    }

    pub fn rename_folder(&self, folder_id: i64, name: &str) -> AppResult<FolderChange> {
        let name = normalize_folder_name(name)?;
        let (database, root) = self.query_source()?;
        for _ in 0..2 {
            let current = folder_service::info(&database, folder_id)?;
            if current.path.is_empty() {
                return Err(AppError::message("不能重命名图库根目录"));
            }
            let new_path = join_folder(parent_path(&current.path), &name);
            let _guard = self.begin_folder_change(
                "重命名文件夹",
                vec![PathScope::write(&current.path), PathScope::write(&new_path)],
            )?;
            let latest = folder_service::info(&database, folder_id)?;
            if latest.path.eq_ignore_ascii_case(&current.path) {
                return self
                    .mutate(|| folder_service::rename(&database, &root, folder_id, &name))
                    .map(folder_change);
            }
        }
        Err(AppError::message("文件夹已变化，请重试"))
    }

    pub fn move_folder(&self, folder_id: i64, parent_id: i64) -> AppResult<FolderChange> {
        let (database, root) = self.query_source()?;
        for _ in 0..2 {
            let current = folder_service::info(&database, folder_id)?;
            if current.path.is_empty() {
                return Err(AppError::message("不能移动图库根目录"));
            }
            let parent = folder_service::info(&database, parent_id)?;
            let leaf = current
                .path
                .rsplit_once('/')
                .map_or(current.path.as_str(), |(_, leaf)| leaf);
            let new_path = join_folder(&parent.path, leaf);
            let _guard = self.begin_folder_change(
                "移动文件夹",
                vec![
                    PathScope::write(&current.path),
                    PathScope::write(&new_path),
                    // 移动会把目标同级序号追加到末尾，同一父级下的并发移动要串行。
                    PathScope::write(&parent.path),
                ],
            )?;
            let latest = folder_service::info(&database, folder_id)?;
            if latest.path.eq_ignore_ascii_case(&current.path) {
                return self
                    .mutate(|| folder_service::move_to(&database, &root, folder_id, parent_id))
                    .map(folder_change);
            }
        }
        Err(AppError::message("文件夹已变化，请重试"))
    }

    pub fn place_folders(
        &self,
        folder_ids: &[i64],
        parent_id: i64,
        sort_index: Option<i64>,
    ) -> AppResult<FolderChange> {
        let (database, root) = self.query_source()?;
        let parent = folder_service::info(&database, parent_id)?;
        let mut paths = vec![PathScope::write(&parent.path)];
        for id in folder_ids {
            let current = folder_service::info(&database, *id)?;
            let leaf = current
                .path
                .rsplit_once('/')
                .map_or(current.path.as_str(), |(_, leaf)| leaf);
            paths.push(PathScope::write(&current.path));
            paths.push(PathScope::write(join_folder(&parent.path, leaf)));
        }
        let _guard = self.begin_folder_change("整理文件夹", paths)?;
        self.mutate(|| {
            folder_service::place_many(&database, &root, folder_ids, parent_id, sort_index)
        })
        .map(folder_change)
    }

    pub fn apply_folder_order(
        &self,
        folder_id: i64,
        key: folder_service::FolderSortKey,
        descending: bool,
        recursive: bool,
        orders: &[(i64, Vec<i64>)],
    ) -> AppResult<FolderChange> {
        let (database, _) = self.query_source()?;
        let current = folder_service::info(&database, folder_id)?;
        let _guard =
            self.begin_folder_change("文件夹排序", vec![PathScope::write(&current.path)])?;
        self.mutate(|| {
            folder_service::apply_order(&database, folder_id, key, descending, recursive, orders)
        })
        .map(folder_change)
    }

    pub fn reorder_folders(
        &self,
        folder_ids: &[i64],
        parent_id: i64,
        sort_index: i64,
    ) -> AppResult<FolderChange> {
        let (database, _) = self.query_source()?;
        let parent = folder_service::info(&database, parent_id)?;
        let _guard =
            self.begin_folder_change("文件夹排序", vec![PathScope::write(&parent.path)])?;
        self.mutate(|| folder_service::reorder(&database, folder_ids, parent_id, sort_index))
            .map(folder_change)
    }

    pub fn delete_folder(&self, folder_id: i64) -> AppResult<FolderChange> {
        let (database, root) = self.query_source()?;
        let current = folder_service::info(&database, folder_id)?;
        let _guard =
            self.begin_folder_change("删除文件夹", vec![PathScope::write(&current.path)])?;
        self.mutate(|| folder_service::delete(&database, &root, folder_id))?;
        Ok(FolderChange {
            folders: Vec::new(),
            removed_ids: vec![folder_id],
        })
    }

    pub fn tags(&self, limit: Option<u32>, include_empty: bool) -> AppResult<Vec<TagInfo>> {
        let (database, _) = self.query_source()?;
        query::list_tags_with_empty(&database, limit, include_empty)
    }

    pub fn search_tags(
        &self,
        query_text: &str,
        limit: Option<u32>,
        include_empty: bool,
    ) -> AppResult<Vec<TagInfo>> {
        let (database, _) = self.query_source()?;
        query::search_tags_with_empty(&database, query_text, limit, include_empty)
    }

    pub fn image_detail(&self, image_id: i64) -> AppResult<ImageDetails> {
        let (database, root) = self.query_source()?;
        metadata::image_details(&database, &root, image_id)
    }

    pub fn image_states(&self, image_ids: &[i64]) -> AppResult<Vec<ImageState>> {
        let (database, _) = self.query_source()?;
        metadata::image_states(&database, image_ids)
    }

    pub fn selection_tags(&self, image_ids: &[i64]) -> AppResult<SelectionTags> {
        let (database, _) = self.query_source()?;
        metadata::selection_tags(&database, image_ids)
    }

    pub fn update_image_score(&self, image_id: i64, score: i64) -> AppResult<()> {
        let _guard = self.begin_metadata_write("修改评分")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::update_score(&database, image_id, score))
    }

    pub fn set_images_score(&self, image_ids: &[i64], score: i64) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改评分")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::set_scores(&database, image_ids, score))
    }

    pub fn update_image_description(&self, image_id: i64, description: &str) -> AppResult<()> {
        let _guard = self.begin_metadata_write("修改描述")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::update_description(&database, image_id, description))
    }

    pub fn set_image_tags(&self, image_id: i64, tag_names: &[String]) -> AppResult<Vec<TagRef>> {
        let _guard = self.begin_metadata_write("修改标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::set_image_tags(&database, image_id, tag_names))
    }

    pub fn add_tags(&self, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::add_tags(&database, image_ids, tag_names))
    }

    pub fn remove_tags(&self, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::remove_tags(&database, image_ids, tag_names))
    }

    pub fn rename_tag(&self, tag_id: i64, name: &str) -> AppResult<()> {
        let _guard = self.begin_metadata_write("重命名标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::rename_tag(&database, tag_id, name))
    }

    pub fn delete_tag(&self, tag_id: i64) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("删除标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::delete_tag(&database, tag_id))
    }

    pub fn merge_tags(&self, source_id: i64, target_id: i64) -> AppResult<metadata::MergeReport> {
        let _guard = self.begin_metadata_write("合并标签")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| {
            metadata::merge_tags_with_id(&database, source_id, target_id).map(|(r, _)| r)
        })
    }

    pub fn move_images(
        &self,
        image_ids: &[i64],
        folder_path: &str,
        reporter: &dyn BatchReporter,
    ) -> AppResult<MoveReport> {
        let (database, root) = self.query_source()?;
        self.mutate(|| metadata::move_images(&database, &root, image_ids, folder_path, reporter))
    }

    pub fn trash_images(&self, image_ids: &[i64]) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("移入回收站")?;
        let (database, _) = self.query_source()?;
        let changed = self.mutate(|| metadata::set_trashed(&database, image_ids, true))?;
        self.ai.invalidate_similarity();
        Ok(changed)
    }

    pub fn restore_images(&self, image_ids: &[i64]) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("恢复图片")?;
        let (database, _) = self.query_source()?;
        let changed = self.mutate(|| metadata::set_trashed(&database, image_ids, false))?;
        self.ai.invalidate_similarity();
        Ok(changed)
    }

    pub fn delete_images(
        &self,
        image_ids: &[i64],
        reporter: &dyn BatchReporter,
    ) -> AppResult<DeleteReport> {
        let (database, _) = self.query_source()?;
        let report = self.mutate(|| metadata::delete_images(&database, image_ids, reporter))?;
        self.ai.invalidate_similarity();
        Ok(report)
    }

    pub fn empty_trash(&self, reporter: &dyn BatchReporter) -> AppResult<DeleteReport> {
        let (database, _) = self.query_source()?;
        let report = self.mutate(|| metadata::empty_trash(&database, reporter))?;
        self.ai.invalidate_similarity();
        Ok(report)
    }

    pub fn collections(&self) -> AppResult<Vec<CollectionInfo>> {
        let (database, _) = self.query_source()?;
        collections::list(&database)
    }

    pub fn create_collection(&self, name: &str) -> AppResult<CollectionInfo> {
        let _guard = self.begin_metadata_write("新建集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::create(&database, name))
    }

    pub fn rename_collection(&self, collection_id: i64, name: &str) -> AppResult<()> {
        let _guard = self.begin_metadata_write("重命名集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::rename(&database, collection_id, name))
    }

    pub fn delete_collection(&self, collection_id: i64) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("删除集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::delete(&database, collection_id))
    }

    pub fn reorder_collections(&self, collection_ids: &[i64]) -> AppResult<()> {
        let _guard = self.begin_metadata_write("整理集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::reorder(&database, collection_ids))
    }

    pub fn add_images_to_collection(
        &self,
        collection_id: i64,
        image_ids: &[i64],
    ) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改集合成员")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::add_images(&database, collection_id, image_ids))
    }

    pub fn remove_images_from_collection(
        &self,
        collection_id: i64,
        image_ids: &[i64],
    ) -> AppResult<u64> {
        let _guard = self.begin_metadata_write("修改集合成员")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::remove_images(&database, collection_id, image_ids))
    }

    pub fn combine_collections(
        &self,
        mode: CombineMode,
        collection_ids: &[i64],
        name: &str,
    ) -> AppResult<CollectionInfo> {
        let _guard = self.begin_metadata_write("合并集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::combine(&database, mode, collection_ids, name))
    }

    pub fn create_collections_from_groups(
        &self,
        groups: &[Vec<i64>],
        base_name: &str,
    ) -> AppResult<Vec<CollectionInfo>> {
        let _guard = self.begin_metadata_write("新建集合")?;
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::create_groups(&database, groups, base_name))
    }

    pub fn history_page(&self) -> AppResult<HistoryPage> {
        let (database, root) = self.query_source()?;
        let entries = history::list(&database)?;
        let (pending_purge_count, pending_purge_bytes) = history::pending_summary(&database)?;
        let storage = history::storage_usage(&database, &root)?;
        let history_task_active = self
            .tasks
            .active()
            .iter()
            .any(|task| matches!(task.kind, TaskKind::Undo | TaskKind::Redo));
        // 有中断标记但撤销/重做任务正在跑，说明是本次运行中的中间态，不算遗留。
        let recovery_needed = !history_task_active && history::has_interrupted_run(&database)?;
        // 撤销与重做要独占时间线、内容写、元数据写与根路径；界面在仍有运行中任务时直接置灰。
        let blocked_reason = if recovery_needed {
            Some("上次撤销或重做异常中断，请重新打开图库恢复".to_owned())
        } else {
            match self.tasks.active().first() {
                Some(task) => Some(format!("「{}」正在进行，完成后才能撤销或重做", task.label)),
                None if entries.is_empty() => Some("还没有操作记录".to_owned()),
                None => None,
            }
        };
        Ok(HistoryPage {
            entries,
            max_steps: self.settings.snapshot().history.max_steps,
            pending_purge_count,
            pending_purge_bytes,
            storage,
            recovery_needed,
            can_run: blocked_reason.is_none(),
            blocked_reason,
        })
    }

    pub fn undo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let (database, _) = self.query_source()?;
        let (status, failed, redo_failed, count) = database.with_reader(|connection| {
            let status: Option<String> = connection
                .query_row(
                    "SELECT status FROM operations WHERE id = ?1",
                    [target_id],
                    |row| row.get(0),
                )
                .optional()?;
            let failed: i64 = connection.query_row(
                "SELECT count(*) FROM operations
                 WHERE id >= ?1 AND status = 'undoFailed'",
                [target_id],
                |row| row.get(0),
            )?;
            let redo_failed: i64 = connection.query_row(
                "SELECT count(*) FROM operations
                 WHERE id >= ?1 AND status IN ('undone', 'redoing') AND error <> ''",
                [target_id],
                |row| row.get(0),
            )?;
            let count: i64 = connection.query_row(
                "SELECT count(*) FROM operations
                 WHERE id >= ?1 AND status IN ('applied', 'undoing')",
                [target_id],
                |row| row.get(0),
            )?;
            Ok((status, failed, redo_failed, count))
        })?;
        match status.as_deref() {
            Some("applied") => {}
            Some("undoFailed") => {
                return Err(AppError::message(
                    "这条记录上次撤销失败，需要先处理失败原因",
                ));
            }
            Some("undone") | Some("redoing") => {
                return Err(AppError::message("这条记录已经撤销，可以重做"));
            }
            Some(_) => return Err(AppError::message("只能撤销已经完成的操作")),
            None => return Err(AppError::message("操作记录不存在")),
        }
        if failed > 0 {
            return Err(AppError::message(
                "之后有一条记录上次撤销失败，不能继续回退；请先处理失败原因或清空记录",
            ));
        }
        if redo_failed > 0 {
            return Err(AppError::message(
                "之后有一条记录上次重做失败，不能继续回退；请先重试重做或清空记录",
            ));
        }
        if count <= 0 {
            return Err(AppError::message("没有可撤销的记录"));
        }
        self.start_undo_task(target_id, count as usize)
    }

    pub fn redo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let (database, _) = self.query_source()?;
        let (status, count) = database.with_reader(|connection| {
            let status: Option<String> = connection
                .query_row(
                    "SELECT status FROM operations WHERE id = ?1",
                    [target_id],
                    |row| row.get(0),
                )
                .optional()?;
            let mut statement = connection
                .prepare("SELECT status FROM operations WHERE id <= ?1 ORDER BY id DESC")?;
            let statuses = statement
                .query_map([target_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let count = statuses
                .iter()
                .take_while(|status| *status == "undone" || *status == "redoing")
                .count();
            Ok((status, count))
        })?;
        match status.as_deref() {
            Some("undone") | Some("redoing") => {}
            Some("applied") => {
                return Err(AppError::message("这条记录还没有撤销，不能重做"));
            }
            Some("undoFailed") => {
                return Err(AppError::message(
                    "这条记录上次撤销失败，需要先处理失败原因",
                ));
            }
            Some(_) => return Err(AppError::message("只能重做已经撤销的操作")),
            None => return Err(AppError::message("操作记录不存在")),
        }
        if count == 0 {
            return Err(AppError::message("没有可重做的记录"));
        }
        self.start_redo_task(target_id, count)
    }

    pub fn clear_history(&self) -> AppResult<TaskStartResult> {
        let (database, root) = self.query_source()?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::ReleaseGarbage,
            "清空操作记录",
        );
        handle.set_cancellable(false);
        let app = self.app.clone();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        self.tasks.launch(
            app,
            handle,
            ResourceRequest::session_read()
                .with_timeline(Access::Write)
                .with_content_writer()
                .with_paths(vec![PathScope::write("")])
                .with_path_note("正在清空操作记录"),
            ConflictPolicy::Fail,
            move |guard| {
                let database = database.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker(
                    "clear-operation-history".to_owned(),
                    task.clone(),
                    move || {
                        let _guard: LaneGuard = guard;
                        task.set_phase(TaskPhase::Running);
                        let result = history::clear(&database, &root)
                            .and_then(|_| history::run_release(&database, &root, task.clone()));
                        query::clear_page_cache();
                        ai.invalidate_similarity();
                        task.finish_from(&result);
                    },
                );
            },
        )
    }

    /// 图库可用后的收尾：先续做中断的撤销（撤销不排队，要先拿到写资源），再按上限裁剪。
    fn after_library_open(&self) {
        self.schedule_recovery();
        self.after_history_write();
    }

    /// 历史或保留步数变化后按上限裁剪、安排释放；失败只记日志。
    ///
    /// 触发点：经 `mutate` 的写命令、编辑保存（命令里没有经过 `mutate`）、
    /// 导入收尾线程、保存设置（步数可能调小）。
    pub(crate) fn after_history_write(&self) {
        let Some((database, _, root)) = self.library_handles() else {
            return;
        };
        let max_steps = self.settings.snapshot().history.max_steps;
        match history::trim(&database, max_steps) {
            Ok(evicted) => {
                // 新写入会删除重做分支，被裁掉的编辑/导入记录也要清掉暂存目录。
                if let Err(error) = history::cleanup_undo_dirs(&database, &root) {
                    log::warn!("清理撤销暂存目录失败：{error}");
                }
                if evicted
                    .iter()
                    .any(|operation| operation.kind.releases_files())
                {
                    self.schedule_release();
                }
            }
            Err(error) => log::warn!("裁剪操作记录失败：{error}"),
        }
    }

    fn schedule_recovery(&self) {
        let plan = {
            let mut guard = self.recovery.lock();
            std::mem::take(&mut *guard)
        };
        let mut resumed = false;
        if let Some(target_id) = plan.resume_undo {
            let Ok((database, _)) = self.query_source() else {
                return;
            };
            let count = database.with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM operations
                     WHERE id >= ?1 AND status IN ('applied', 'undoing')",
                    [target_id],
                    |row| row.get::<_, i64>(0),
                )?)
            });
            match count {
                Ok(count) if count > 0 => {
                    if let Err(error) = self.start_undo_task(target_id, count as usize) {
                        log::warn!("恢复中断的撤销失败：{error}");
                    } else {
                        resumed = true;
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("读取未完成撤销记录失败：{error}"),
            }
        }
        if !resumed && let Some(target_id) = plan.resume_redo {
            let Ok((database, _)) = self.query_source() else {
                return;
            };
            let count = database.with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM operations
                     WHERE id <= ?1 AND status = 'redoing'",
                    [target_id],
                    |row| row.get::<_, i64>(0),
                )?)
            });
            match count {
                Ok(count) if count > 0 => {
                    if let Err(error) = self.start_redo_task(target_id, count as usize) {
                        log::warn!("恢复中断的重做失败：{error}");
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("读取未完成重做记录失败：{error}"),
            }
        }
        if plan.release_needed {
            self.schedule_release();
        }
    }

    fn schedule_release(&self) {
        if self
            .tasks
            .active()
            .iter()
            .any(|task| task.kind == TaskKind::ReleaseGarbage)
        {
            return;
        }
        let Some((database, _, _)) = self.library_handles() else {
            return;
        };
        match history::has_orphan_purge(&database) {
            Ok(true) => {
                if let Err(error) = self.start_release_task("释放删除文件", ConflictPolicy::Queue)
                {
                    log::warn!("安排释放任务失败：{error}");
                }
            }
            Ok(false) => {}
            Err(error) => log::warn!("检查待释放文件失败：{error}"),
        }
    }

    fn start_undo_task(&self, target_id: i64, count: usize) -> AppResult<TaskStartResult> {
        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::Undo,
            format!("撤销 {count} 条记录"),
        );
        handle.set_cancellable(count > 1);
        let app = self.app.clone();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Write)
            .with_content_writer()
            .with_metadata_writer()
            .with_paths(vec![PathScope::write("")])
            .with_path_note("正在撤销操作");
        self.tasks
            .launch(app, handle, request, ConflictPolicy::Fail, move |guard| {
                let database = database.clone();
                let thumbs = thumbs.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker("undo-history".to_owned(), task.clone(), move || {
                    let _guard: LaneGuard = guard;
                    task.set_phase(TaskPhase::Running);
                    let result =
                        history::run_undo(&database, &root, &thumbs, target_id, task.clone());
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    task.finish_from(&result);
                });
            })
    }

    fn start_redo_task(&self, target_id: i64, count: usize) -> AppResult<TaskStartResult> {
        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::Redo,
            format!("重做 {count} 条记录"),
        );
        handle.set_cancellable(count > 1);
        let app = self.app.clone();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Write)
            .with_content_writer()
            .with_metadata_writer()
            .with_paths(vec![PathScope::write("")])
            .with_path_note("正在重做操作");
        self.tasks
            .launch(app, handle, request, ConflictPolicy::Fail, move |guard| {
                let database = database.clone();
                let thumbs = thumbs.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker("redo-history".to_owned(), task.clone(), move || {
                    let _guard: LaneGuard = guard;
                    task.set_phase(TaskPhase::Running);
                    let result =
                        history::run_redo(&database, &root, &thumbs, target_id, task.clone());
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    task.finish_from(&result);
                });
            })
    }

    fn start_release_task(
        &self,
        scope: &str,
        policy: ConflictPolicy,
    ) -> AppResult<TaskStartResult> {
        let (database, root) = self.query_source()?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::ReleaseGarbage,
            scope,
        );
        handle.set_cancellable(false);
        let app = self.app.clone();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let request = ResourceRequest::session_read()
            .with_content_writer()
            .with_paths(vec![PathScope::write("")])
            .with_path_note("正在释放删除文件");
        self.tasks
            .launch(app, handle, request, policy, move |guard| {
                let database = database.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker("release-garbage".to_owned(), task.clone(), move || {
                    let _guard: LaneGuard = guard;
                    task.set_phase(TaskPhase::Running);
                    let result = history::run_release(&database, &root, task.clone());
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    task.finish_from(&result);
                });
            })
    }

    pub fn image_pose(&self, image_id: i64) -> AppResult<Option<PoseData>> {
        self.ai.read_pose(image_id)
    }

    pub(crate) fn begin_edit(&self, image_id: i64) -> AppResult<ResourceGuard> {
        let (database, _) = self.query_source()?;
        for _ in 0..3 {
            let row = content::load(&database, image_id)?;
            let guard =
                self.begin_content_write("保存编辑", vec![PathScope::write(&row.folder_path)])?;
            let latest = content::load(&database, image_id)?;
            if latest.folder_path.eq_ignore_ascii_case(&row.folder_path)
                && latest.content_hash == row.content_hash
            {
                self.ai.invalidate_similarity();
                query::clear_page_cache();
                return Ok(guard);
            }
        }
        Err(AppError::message("图片位置正在变化，请稍后重试"))
    }

    pub(crate) fn edit_handles(&self) -> AppResult<(Arc<LibraryDb>, Arc<ThumbPack>, PathBuf)> {
        self.library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))
    }

    pub(crate) fn copy_source(&self, image_id: i64) -> AppResult<(PathBuf, ResourceGuard)> {
        let (database, root) = self.query_source()?;
        let holder = Self::short_holder("复制图片", "正在读取图片");
        let (row, guard) =
            content::try_lock_original(&database, &self.resources(), image_id, &holder)?;
        let path = row.path(&crate::library::paths::LibraryPaths::new(&root))?;
        Ok((path, guard))
    }

    pub fn cancel_task(&self, task_id: &str) -> AppResult<()> {
        self.tasks.cancel(task_id)
    }

    pub fn image_capabilities(&self, image_ids: &[i64]) -> AppResult<Vec<ImageCapabilities>> {
        let (database, _) = self.query_source()?;
        capability::image_capabilities(&database, image_ids)
    }

    pub fn feature_summary(&self) -> AppResult<FeatureSummary> {
        let (database, _) = self.query_source()?;
        capability::feature_summary(&database)
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
            // 「立即回收」与空闲循环提交的是同一个任务，区别只在提交前是否等空闲条件。
            BatchTaskKind::PackCompact => self.compact_pack_now(app),
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
                        for image_id in pending {
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
                                }
                                Ok(false) => {
                                    worker.inc_skipped();
                                    consecutive_failures = 0;
                                }
                                Err(error) => {
                                    worker.inc_failed();
                                    worker.add_failure(Some(image_id), "", error.to_string());
                                    consecutive_failures += 1;
                                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                                        worker.set_message("连续多张缩略图处理失败，任务已停止");
                                        break;
                                    }
                                }
                            }
                        }
                        worker.finish_from(&Ok(()));
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
                            // 报告里的失败清单要进任务胶囊，否则用户只知道失败了几张。
                            for failure in &report.failures {
                                worker.add_failure(
                                    Some(failure.image_id),
                                    &failure.path,
                                    &failure.reason,
                                );
                            }
                            worker.set_message(&summary);
                            if report.stop_reason.is_some() {
                                worker.finish(TaskPhase::Failed);
                            } else {
                                worker.finish_from(&Ok(()));
                            }
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
                        matches!(activity::idle_for(), Some(idle) if idle < idle_loop::IDLE_THRESHOLD)
                    };
                    match pack_gc::compact(&database, &root, &thumbs, &*worker, &user_back) {
                        Ok(report) => {
                            let freed = format_savings(report.freed_bytes);
                            let message = match report.stop {
                                pack_gc::CompactStop::Done => {
                                    format!("已回收 {} 个段 · 释放 {freed}", report.segments)
                                }
                                pack_gc::CompactStop::Yielded => format!(
                                    "已让位 · 已回收 {} 个段 · 释放 {freed}，剩余部分下次空闲继续",
                                    report.segments
                                ),
                                pack_gc::CompactStop::Cancelled => {
                                    format!("已停止 · 已回收 {} 个段 · 释放 {freed}", report.segments)
                                }
                            };
                            worker.set_message(&message);
                            match report.stop {
                                pack_gc::CompactStop::Yielded => {
                                    worker.finish(TaskPhase::Yielded)
                                }
                                pack_gc::CompactStop::Cancelled => {
                                    worker.finish(TaskPhase::Cancelled)
                                }
                                pack_gc::CompactStop::Done => {
                                    worker.finish_from(&Ok::<(), AppError>(()))
                                }
                            }
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

    /// 相似分组合并前的保留项挑选：只读，返回与 `groups` 等长的保留 id。
    pub fn similarity_group_keeps(
        &self,
        groups: &[Vec<i64>],
        sort: &[query::SortSpec],
    ) -> AppResult<Vec<Option<i64>>> {
        let (database, _) = self.query_source()?;
        query::group_keeps(&database, groups, sort)
    }
}

fn conflict_sentence(conflicts: &[Conflict]) -> String {
    conflicts
        .first()
        .map(Conflict::sentence)
        .unwrap_or_else(|| "有操作正在进行".to_owned())
}

/// 移动图片的任务级路径写：源文件夹与目标文件夹；源文件夹过多时退化为根写。
fn move_resource_request(
    database: &LibraryDb,
    image_ids: &[i64],
    target: &str,
) -> AppResult<ResourceRequest> {
    let mut sources = std::collections::HashSet::new();
    for chunk in image_ids.chunks(400) {
        let sql = format!(
            "SELECT DISTINCT folders.path FROM images
             JOIN folders ON folders.id = images.folder_id
             WHERE images.id IN ({})",
            crate::library::placeholders(chunk.len())
        );
        database.with_reader(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                row.get::<_, String>(0)
            })?;
            for path in rows {
                sources.insert(path?);
            }
            Ok(())
        })?;
    }
    let mut paths = if sources.is_empty() || sources.len() > 64 {
        vec![PathScope::write("")]
    } else {
        sources.into_iter().map(PathScope::write).collect()
    };
    paths.push(PathScope::write(normalize_folder(target)?));
    Ok(ResourceRequest::session_read()
        .with_timeline(Access::Read)
        .with_content_writer()
        .with_paths(paths)
        .with_path_note("正在移动图片"))
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

fn conflict_error(active: &[TaskSummary]) -> AppError {
    let label = active
        .first()
        .map(|task| task.label.as_str())
        .unwrap_or("其他任务");
    AppError::message(format!("「{label}」正在进行，请等待完成或取消"))
}

/// 文件夹写命令只回传受影响的行，前端按 id 就地合并。
fn folder_change(folders: Vec<FolderInfo>) -> FolderChange {
    FolderChange {
        folders,
        removed_ids: Vec::new(),
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
                let mut statement = connection
                    .prepare("SELECT id FROM folders WHERE id = ?1 OR path LIKE ?2 ESCAPE '\\'")?;
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

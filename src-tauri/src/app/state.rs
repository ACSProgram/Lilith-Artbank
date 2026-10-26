use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
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
        error::{AppError, AppResult},
        settings::SettingsStore,
        tasks::{
            BatchTaskKind, BatchTaskRequest, ConflictPolicy, LaneGuard, TaskBus, TaskHandle,
            TaskKind, TaskLane, TaskPhase, TaskSnapshot, TaskStartResult, TaskSummary,
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
        manifest::{LibraryInspection, inspect},
        metadata::{self, DeleteReport, ImageDetails, MoveReport, TagRef},
        optimize_estimate::{self, OptimizeEstimate},
        query::{self, FilterCounts, FolderInfo, ImagePage, LibraryQuery, TagInfo},
        service::{self, LibraryInfo, LibrarySession},
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
    library: Mutex<Option<LibrarySession>>,
    import_job: Mutex<Option<Arc<ImportJob>>>,
    /// 已结束的导入回执；MCP 任务列表在导入结束后仍要能读到统计与预览。
    import_receipts: Mutex<VecDeque<ImportSnapshot>>,
    managed: AtomicBool,
    recovery: Mutex<RecoveryPlan>,
    optimize_estimates: Arc<Mutex<VecDeque<(String, OptimizeEstimate)>>>,
    mcp_sheets: Arc<Mutex<SheetCache>>,
    mcp_write: Arc<Mutex<()>>,
    mcp_approvals: Arc<ApprovalQueue>,
    mcp_tasks: Arc<Mutex<VecDeque<String>>>,
}

/// MCP 能取消的任务只保留最近这些条，足够覆盖一次确认后的等待窗口。
const MAX_MCP_TASKS: usize = 100;
/// 导入回执与任务列表保留同样量级，旧的滚动淘汰。
const MAX_IMPORT_RECEIPTS: usize = 20;

/// 缩略图重建连续失败达到该数量即停止，避免把整库都记成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;

impl AppState {
    pub fn new(settings: SettingsStore, app: AppHandle) -> Self {
        let mcp_approvals = Arc::new(ApprovalQueue::new(app.clone()));
        Self {
            settings,
            ai: AiRuntime::new(),
            tasks: TaskBus::new(),
            mcp: McpRuntime::new(),
            app,
            library: Mutex::new(None),
            import_job: Mutex::new(None),
            import_receipts: Mutex::new(VecDeque::new()),
            managed: AtomicBool::new(false),
            recovery: Mutex::new(RecoveryPlan::default()),
            optimize_estimates: Arc::new(Mutex::new(VecDeque::new())),
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

    pub fn status(&self) -> AppResult<Option<LibraryInfo>> {
        let snapshot = self
            .library
            .lock()
            .as_ref()
            .map(|session| (session.info.clone(), session.db.clone()));
        let Some((mut info, database)) = snapshot else {
            return Ok(None);
        };
        let (image_count, trash_count, temp_count) = database.counts()?;
        info.image_count = image_count;
        info.trash_count = trash_count;
        info.temp_count = temp_count;
        Ok(Some(info))
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
        self.ensure_no_activity()?;
        let mut request = request.normalized()?;
        request.worker_threads = self.settings.snapshot().performance.worker_threads;

        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;

        let temp_target = request.target == ImportTarget::Temp;
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
        let operation_id = if temp_target {
            None
        } else {
            Some(history::begin_live(
                &database,
                NewOperation::new(HistoryKind::Import, label),
            )?)
        };
        handle.set_operation_id(operation_id);
        let source = ImportSource {
            database: database.clone(),
            thumbs,
            root,
            operation_id,
        };
        let started = match self
            .tasks
            .launch(app, handle, ConflictPolicy::Fail, move |guard| {
                crate::import::start(runner_app, source, runner_job, request, guard);
            }) {
            Ok(result) => result,
            Err(error) => {
                if let Some(operation_id) = operation_id {
                    let _ = history::finish_live(&database, operation_id, &error.to_string());
                }
                return Err(error);
            }
        };
        if let TaskStartResult::Conflict { active } = started {
            // 冲突时导入线程没有启动；空记录在这里收掉。
            if let Some(operation_id) = operation_id {
                let _ = history::finish_live(&database, operation_id, "");
            }
            return Err(conflict_error(&active));
        }
        *self.import_job.lock() = Some(job.clone());
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
        let slot = self.import_job.lock();
        let job = slot
            .as_ref()
            .filter(|job| job.id() == job_id)
            .ok_or_else(|| AppError::message("导入任务不存在或已被替换"))?;
        job.request_cancel();
        Ok(())
    }

    pub fn import_status(&self, job_id: &str) -> Option<ImportSnapshot> {
        self.import_job
            .lock()
            .as_ref()
            .filter(|job| job.id() == job_id)
            .map(|job| job.snapshot())
    }

    fn ensure_no_activity(&self) -> AppResult<()> {
        if !self.tasks.any_busy() {
            return Ok(());
        }
        if let Some(task) = self.tasks.active().into_iter().next() {
            return Err(AppError::message(format!(
                "「{}」正在进行，请等待完成或取消",
                task.label
            )));
        }
        Err(AppError::message("有操作正在进行，请等待完成"))
    }

    fn begin_maintenance(&self) -> AppResult<LaneGuard> {
        self.ensure_no_activity()?;
        self.tasks.acquire_immediate(TaskLane::Write)
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
        })
    }

    /// MCP 写入在本进程内串行；与任务总线无关，只保护多个客户端同时调用。
    pub(crate) fn lock_mcp_writes(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.mcp_write.lock()
    }

    /// 短写入立即占用 write lane；任何在跑的任务（导入、移动、编辑、优化、撤销/重做）都会拒绝。
    pub(crate) fn acquire_write_lane(&self) -> AppResult<LaneGuard> {
        self.tasks.acquire_immediate(TaskLane::Write)
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
            ConflictPolicy::Fail,
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
        self.ensure_no_activity()?;
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
        *self.library.lock() = Some(session);
        *self.recovery.lock() = RecoveryPlan::default();
        self.optimize_estimates.lock().clear();
        self.import_receipts.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        Ok(info)
    }

    pub fn open_library(&self, path: &Path) -> AppResult<LibraryInfo> {
        self.ensure_no_activity()?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let mut session = service::open(path, read_connections)?;
        let plan = history::recover_on_open(&session.db)?;
        if let Err(error) = history::cleanup_undo_dirs(&session.db, Path::new(&session.info.path)) {
            log::warn!("清理撤销暂存目录失败：{error}");
        }
        // 上次退出没能清掉的临时图片在这里补清；计数要按清理后的状态重算。
        if let Err(error) = temp::clear(&session.db, Path::new(&session.info.path)) {
            log::warn!("清空临时文件夹失败：{error}");
        }
        let (image_count, trash_count, temp_count) = session.db.counts()?;
        session.info.image_count = image_count;
        session.info.trash_count = trash_count;
        session.info.temp_count = temp_count;
        let info = session.info.clone();
        self.settings.set_library_path(Some(info.path.clone()))?;
        self.clear_previous_temp();
        self.ai.library_opened(
            info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            PathBuf::from(&info.path),
        );
        *self.library.lock() = Some(session);
        *self.recovery.lock() = plan;
        self.optimize_estimates.lock().clear();
        self.import_receipts.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        if self.managed.load(Ordering::SeqCst) {
            self.after_library_open();
        }
        Ok(info)
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
        // 写任务不能中途关库；AI/扫描任务先取消，等它们真正退出后再释放句柄。
        if self.tasks.lane_busy(TaskLane::Write) {
            let label = self
                .tasks
                .active()
                .into_iter()
                .find(|task| task.lane == TaskLane::Write)
                .map(|task| task.label)
                .unwrap_or_else(|| "文件任务".to_owned());
            return Err(AppError::message(format!(
                "「{label}」正在进行，请等待完成或取消后再关闭图库"
            )));
        }
        self.tasks.cancel_lanes(&[TaskLane::Ai, TaskLane::Scan]);
        if !self.tasks.wait_idle(Duration::from_secs(10)) {
            return Err(AppError::message("任务正在停止，请稍后再关闭图库"));
        }
        if let Some((database, _, root)) = self.library_handles()
            && let Err(error) = temp::clear(&database, &root)
        {
            log::warn!("关闭图库前清空临时文件夹失败：{error}");
        }
        self.ai.library_closed();
        *self.library.lock() = None;
        *self.recovery.lock() = RecoveryPlan::default();
        self.optimize_estimates.lock().clear();
        self.import_receipts.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        self.settings.set_library_path(None)?;
        Ok(())
    }

    /// 应用退出：没有写任务时尽量清空临时区；来不及清的下次打开图库会补清。
    pub fn shutdown(&self) {
        if self.tasks.lane_busy(TaskLane::Write) {
            log::warn!("退出时仍有写任务，临时文件夹留到下次打开图库时清空");
            return;
        }
        self.tasks.cancel_lanes(&[TaskLane::Ai, TaskLane::Scan]);
        if !self.tasks.wait_idle(Duration::from_secs(2)) {
            log::warn!("退出时任务未停止，临时文件夹留到下次打开图库时清空");
            return;
        }
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

    pub fn filter_counts(&self, query: LibraryQuery) -> AppResult<FilterCounts> {
        let (database, _) = self.query_source()?;
        query::filter_counts(&database, query)
    }

    /// 只算命中总数，不取分面；对话框要一个“当前视图 N 张”时用它，别跑完整统计。
    pub fn query_total(&self, query: LibraryQuery) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        query::query_total(&database, query)
    }

    pub fn folders(&self) -> AppResult<Vec<FolderInfo>> {
        let (database, _) = self.query_source()?;
        query::list_folders(&database)
    }

    pub fn create_folder(&self, path: &str) -> AppResult<Vec<FolderInfo>> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        self.mutate(|| folder_service::create(&database, &root, path))
    }

    pub fn rename_folder(&self, folder_id: i64, name: &str) -> AppResult<Vec<FolderInfo>> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        self.mutate(|| folder_service::rename(&database, &root, folder_id, name))
    }

    pub fn move_folder(&self, folder_id: i64, parent_id: i64) -> AppResult<Vec<FolderInfo>> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        self.mutate(|| folder_service::move_to(&database, &root, folder_id, parent_id))
    }

    pub fn reorder_folders(
        &self,
        folder_ids: &[i64],
        parent_id: i64,
        sort_index: i64,
    ) -> AppResult<Vec<FolderInfo>> {
        let _guard = self.begin_maintenance()?;
        let (database, _) = self.query_source()?;
        self.mutate(|| folder_service::reorder(&database, folder_ids, parent_id, sort_index))
    }

    pub fn delete_folder(&self, folder_id: i64) -> AppResult<Vec<FolderInfo>> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        self.mutate(|| folder_service::delete(&database, &root, folder_id))
    }

    pub fn tags(&self, limit: Option<u32>) -> AppResult<Vec<TagInfo>> {
        let (database, _) = self.query_source()?;
        query::list_tags(&database, limit)
    }

    pub fn search_tags(&self, query_text: &str, limit: Option<u32>) -> AppResult<Vec<TagInfo>> {
        let (database, _) = self.query_source()?;
        query::search_tags(&database, query_text, limit)
    }

    pub fn image_detail(&self, image_id: i64) -> AppResult<ImageDetails> {
        let (database, root) = self.query_source()?;
        metadata::image_details(&database, &root, image_id)
    }

    pub fn update_image_score(&self, image_id: i64, score: i64) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::update_score(&database, image_id, score))
    }

    pub fn set_images_score(&self, image_ids: &[i64], score: i64) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::set_scores(&database, image_ids, score))
    }

    pub fn update_image_description(&self, image_id: i64, description: &str) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::update_description(&database, image_id, description))
    }

    pub fn set_image_tags(&self, image_id: i64, tag_names: &[String]) -> AppResult<Vec<TagRef>> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::set_image_tags(&database, image_id, tag_names))
    }

    pub fn add_tags(&self, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::add_tags(&database, image_ids, tag_names))
    }

    pub fn remove_tags(&self, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::remove_tags(&database, image_ids, tag_names))
    }

    pub fn rename_tag(&self, tag_id: i64, name: &str) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::rename_tag(&database, tag_id, name))
    }

    pub fn delete_tag(&self, tag_id: i64) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        self.mutate(|| metadata::delete_tag(&database, tag_id))
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
        let (database, _) = self.query_source()?;
        let changed = self.mutate(|| metadata::set_trashed(&database, image_ids, true))?;
        self.ai.invalidate_similarity();
        Ok(changed)
    }

    pub fn restore_images(&self, image_ids: &[i64]) -> AppResult<u64> {
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
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::create(&database, name))
    }

    pub fn rename_collection(&self, collection_id: i64, name: &str) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::rename(&database, collection_id, name))
    }

    pub fn delete_collection(&self, collection_id: i64) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::delete(&database, collection_id))
    }

    pub fn reorder_collections(&self, collection_ids: &[i64]) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::reorder(&database, collection_ids))
    }

    pub fn add_images_to_collection(
        &self,
        collection_id: i64,
        image_ids: &[i64],
    ) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::add_images(&database, collection_id, image_ids))
    }

    pub fn remove_images_from_collection(
        &self,
        collection_id: i64,
        image_ids: &[i64],
    ) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::remove_images(&database, collection_id, image_ids))
    }

    pub fn combine_collections(
        &self,
        mode: CombineMode,
        collection_ids: &[i64],
        name: &str,
    ) -> AppResult<CollectionInfo> {
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::combine(&database, mode, collection_ids, name))
    }

    pub fn create_collections_from_groups(
        &self,
        groups: &[Vec<i64>],
        base_name: &str,
    ) -> AppResult<Vec<CollectionInfo>> {
        let (database, _) = self.query_source()?;
        self.mutate(|| collections::create_groups(&database, groups, base_name))
    }

    pub fn history_page(&self) -> AppResult<HistoryPage> {
        let (database, _) = self.query_source()?;
        let entries = history::list(&database)?;
        let (pending_purge_count, pending_purge_bytes) = history::pending_summary(&database)?;
        // 撤销与重做都要独占 write lane，任何在跑的任务（包括 AI 与扫描）都会让它排队或失败。
        let blocked_reason = match self.tasks.active().first() {
            Some(task) => Some(format!("「{}」正在进行，完成后才能撤销或重做", task.label)),
            None if entries.is_empty() => Some("还没有操作记录".to_owned()),
            None => None,
        };
        Ok(HistoryPage {
            entries,
            max_steps: self.settings.snapshot().history.max_steps,
            pending_purge_count,
            pending_purge_bytes,
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
        self.tasks
            .launch(app, handle, ConflictPolicy::Fail, move |guard| {
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
            })
    }

    /// 图库可用后的收尾：先续做中断的撤销（撤销不可排队，要先占 write lane），再按上限裁剪。
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
        self.tasks
            .launch(app, handle, ConflictPolicy::Fail, move |guard| {
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
        self.tasks
            .launch(app, handle, ConflictPolicy::Fail, move |guard| {
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
        self.tasks.launch(app, handle, policy, move |guard| {
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

    pub(crate) fn begin_edit(&self) -> AppResult<LaneGuard> {
        let guard = self.begin_maintenance()?;
        self.ai.invalidate_similarity();
        query::clear_page_cache();
        Ok(guard)
    }

    pub(crate) fn edit_handles(&self) -> AppResult<(Arc<LibraryDb>, Arc<ThumbPack>, PathBuf)> {
        self.library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))
    }

    pub(crate) fn copy_source(&self, image_id: i64) -> AppResult<PathBuf> {
        let (database, root) = self.query_source()?;
        content::original_path(&database, &root, image_id)
    }

    /// 启动一个会修改文件/缩略图的后台任务（写入 lane，独占）。
    pub fn begin_write_task(
        &self,
        app: &AppHandle,
        kind: TaskKind,
        scope: String,
    ) -> AppResult<(Arc<TaskHandle>, LaneGuard)> {
        let handle = TaskHandle::new(uuid::Uuid::new_v4().to_string(), kind, scope);
        let guard = self.tasks.begin_now(app, handle.clone())?;
        Ok((handle, guard))
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

    /// 临时区特征生成：只接受已校验的临时图片 id，占用 write lane，不写操作记录。
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
            ConflictPolicy::Fail,
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
                request.conflict,
            ),
            BatchTaskKind::OptimizeEstimate => self.start_optimize_estimate(app, request),
            BatchTaskKind::Optimize => self.start_optimize_task(app, request),
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
        self.tasks
            .launch(app, handle, request.conflict, move |guard| {
                let _ =
                    tasks.spawn_worker("thumbnail-rebuild".to_owned(), worker.clone(), move || {
                        // 写入 lane 由这个线程持有，直到所有图片处理结束。
                        let _guard: LaneGuard = guard;
                        worker.set_phase(TaskPhase::Running);
                        worker.set_total(ids.len());
                        let mut consecutive_failures = 0_usize;
                        for image_id in ids {
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
            })
    }

    /// 空间优化估算：只读扫描与抽样编码，结果按任务 id 缓存在内存里。
    fn start_optimize_estimate(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        let options = request.optimize.clone().unwrap_or_default();
        options.validate()?;
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
        self.tasks
            .launch(app, handle, request.conflict, move |guard| {
                let _ =
                    tasks.spawn_worker("optimize-estimate".to_owned(), worker.clone(), move || {
                        let _guard: LaneGuard = guard;
                        worker.set_phase(TaskPhase::Running);
                        let result = optimize_estimate::estimate(
                            &database,
                            &root,
                            &task_id,
                            &ids,
                            &options,
                            sample_size,
                            &fingerprint,
                            &*worker,
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
            })
    }

    /// 空间优化执行：先按估算凭证核对范围与参数，再在写入 lane 上编码并发布。
    fn start_optimize_task(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        let options = request.optimize.clone().unwrap_or_default();
        options.validate()?;
        let estimate_id = request
            .estimate_id
            .as_deref()
            .ok_or_else(|| AppError::message("请先估算压缩空间，再确认执行"))?;
        let estimate = self
            .optimize_estimate(estimate_id)
            .ok_or_else(|| AppError::message("估算结果已过期，请重新估算"))?;

        let (database, thumbs, root) = self.edit_handles()?;
        let fingerprint = optimize_fingerprint(&request.scope, &options);
        let ids = request.scope.resolve(&database)?;
        if ids.is_empty() {
            return Err(AppError::message("当前范围没有图片"));
        }
        let candidates = crate::library::optimize::collect_candidates(&database, &ids)?;
        let (count, bytes) = crate::library::optimize::eligible_summary(&candidates, &options);
        // 指纹与候选漂移都在这里一次判定；没有候选时 `verify_grant` 自己会报错。
        crate::library::optimize::verify_grant(&estimate, &fingerprint, count, bytes)?;

        let operation_id = history::begin_live(
            &database,
            NewOperation::new(
                HistoryKind::Optimize,
                format!("优化图片空间 · {}", request.scope.label()),
            ),
        )?;

        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::Optimize,
            request.scope.label(),
        );
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let cleanup_database = database.clone();
        let launched = self
            .tasks
            .launch(app, handle, request.conflict, move |guard| {
                let _ = tasks.spawn_worker("optimize".to_owned(), worker.clone(), move || {
                    let _guard: LaneGuard = guard;
                    worker.set_phase(TaskPhase::Running);
                    worker.set_total(count as usize);
                    let result = crate::library::optimize::optimize(
                        &database,
                        &root,
                        &thumbs,
                        candidates,
                        &options,
                        operation_id,
                        &*worker,
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
                            let _ = history::finish_live(&database, operation_id, &error);
                            let mut summary = format!(
                                "处理 {} 张，省 {}",
                                report.processed,
                                format_savings(report.savings_bytes)
                            );
                            if report.skipped > 0 {
                                summary.push_str(&format!("，跳过 {} 张", report.skipped));
                            }
                            if report.failed > 0 {
                                summary.push_str(&format!("，失败 {} 张", report.failed));
                            }
                            worker.set_message(&summary);
                            if report.stop_reason.is_some() {
                                worker.finish(TaskPhase::Failed);
                            } else {
                                worker.finish_from(&Ok(()));
                            }
                        }
                        Err(error) => {
                            let _ =
                                history::finish_live(&database, operation_id, &error.to_string());
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                        }
                    }
                });
            });

        // 任务没能启动（冲突或线程失败）时不能留下一条永远不会结束的记录。
        if matches!(launched, Ok(TaskStartResult::Conflict { .. }) | Err(_)) {
            let _ = history::finish_live(&cleanup_database, operation_id, "");
        }
        launched
    }

    pub fn optimize_estimate(&self, task_id: &str) -> Option<OptimizeEstimate> {
        self.optimize_estimates
            .lock()
            .iter()
            .find(|(id, _)| id == task_id)
            .map(|(_, estimate)| estimate.clone())
    }

    pub fn tasks_snapshot(&self) -> Vec<TaskSnapshot> {
        self.tasks.list()
    }

    pub fn similarity_groups_result(&self, task_id: &str) -> Option<SimilarityGroupsResult> {
        self.ai.groups_result(task_id)
    }
}

fn store_estimate(store: &Mutex<VecDeque<(String, OptimizeEstimate)>>, estimate: OptimizeEstimate) {
    const MAX_ESTIMATES: usize = 10;
    let mut entries = store.lock();
    entries.push_front((estimate.task_id.clone(), estimate));
    entries.truncate(MAX_ESTIMATES);
}

/// 估算与执行共用同一份参数指纹；批次 2 的执行命令据此校验估算凭证。
fn optimize_fingerprint(scope: &crate::app::tasks::TaskScope, options: &OptimizeOptions) -> String {
    let options = serde_json::to_string(options).unwrap_or_default();
    blake3::hash(format!("{scope:?}|{options}").as_bytes())
        .to_hex()
        .to_string()
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

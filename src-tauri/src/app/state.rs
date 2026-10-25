use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use tauri::AppHandle;

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
    import::{
        ImportRequest, ImportSource,
        job::{ImportJob, ImportSnapshot},
    },
    library::{
        BatchReporter,
        capability::{self, FeatureSummary, ImageCapabilities},
        collections::{self, CollectionInfo, CombineMode},
        content,
        db::LibraryDb,
        folders as folder_service,
        history::{self, HistoryPage, RecoveryPlan},
        manifest::{LibraryInspection, inspect},
        metadata::{self, DeleteReport, ImageDetails, MoveReport, TagRef},
        query::{self, FilterCounts, FolderInfo, ImagePage, LibraryQuery, TagInfo},
        service::{self, LibraryInfo, LibrarySession},
        thumbs::{self, ThumbPack},
        viewer::PoseData,
    },
};

pub struct AppState {
    pub settings: SettingsStore,
    pub ai: AiRuntime,
    pub tasks: TaskBus,
    app: AppHandle,
    library: Mutex<Option<LibrarySession>>,
    import_job: Mutex<Option<Arc<ImportJob>>>,
    managed: AtomicBool,
    recovery: Mutex<RecoveryPlan>,
}

/// 缩略图重建连续失败达到该数量即停止，避免把整库都记成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;

impl AppState {
    pub fn new(settings: SettingsStore, app: AppHandle) -> Self {
        Self {
            settings,
            ai: AiRuntime::new(),
            tasks: TaskBus::new(),
            app,
            library: Mutex::new(None),
            import_job: Mutex::new(None),
            managed: AtomicBool::new(false),
            recovery: Mutex::new(RecoveryPlan::default()),
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
        let (image_count, trash_count) = database.counts()?;
        info.image_count = image_count;
        info.trash_count = trash_count;
        Ok(Some(info))
    }

    pub fn start_import(
        &self,
        app: AppHandle,
        request: ImportRequest,
    ) -> AppResult<ImportSnapshot> {
        self.ensure_no_activity()?;
        let mut request = request.normalized()?;
        request.worker_threads = self.settings.snapshot().performance.worker_threads;

        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;

        let scope = format!("{} 个路径", request.paths.len());
        let job = Arc::new(ImportJob::new(uuid::Uuid::new_v4().to_string(), scope));
        let handle = job.handle();
        let runner_job = job.clone();
        let runner_app = app.clone();
        let source = ImportSource {
            database,
            thumbs,
            root,
        };
        let started = self
            .tasks
            .launch(app, handle, ConflictPolicy::Fail, move |guard| {
                crate::import::start(runner_app, source, runner_job, request, guard);
            })?;
        if let TaskStartResult::Conflict { active } = started {
            return Err(conflict_error(&active));
        }
        *self.import_job.lock() = Some(job.clone());
        Ok(job.snapshot())
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
    }

    pub fn create_library(&self, path: &Path, name: &str) -> AppResult<LibraryInfo> {
        self.ensure_no_activity()?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let (session, _manifest) = service::create(path, name, read_connections)?;
        let info = session.info.clone();
        self.settings.set_library_path(Some(info.path.clone()))?;
        self.ai.library_opened(
            info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            PathBuf::from(&info.path),
        );
        *self.library.lock() = Some(session);
        *self.recovery.lock() = RecoveryPlan::default();
        query::clear_page_cache();
        Ok(info)
    }

    pub fn open_library(&self, path: &Path) -> AppResult<LibraryInfo> {
        self.ensure_no_activity()?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let session = service::open(path, read_connections)?;
        let info = session.info.clone();
        let plan = history::recover_on_open(&session.db)?;
        if let Err(error) = history::cleanup_undo_dirs(&session.db, Path::new(&info.path)) {
            log::warn!("清理撤销暂存目录失败：{error}");
        }
        self.settings.set_library_path(Some(info.path.clone()))?;
        self.ai.library_opened(
            info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            PathBuf::from(&info.path),
        );
        *self.library.lock() = Some(session);
        *self.recovery.lock() = plan;
        query::clear_page_cache();
        if self.managed.load(Ordering::SeqCst) {
            self.after_library_open();
        }
        Ok(info)
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
        self.ai.library_closed();
        *self.library.lock() = None;
        *self.recovery.lock() = RecoveryPlan::default();
        query::clear_page_cache();
        self.settings.set_library_path(None)?;
        Ok(())
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

    pub fn history_page(&self) -> AppResult<HistoryPage> {
        let (database, _) = self.query_source()?;
        let entries = history::list(&database)?;
        let (pending_purge_count, pending_purge_bytes) = history::pending_summary(&database)?;
        // 撤销要独占 write lane，任何在跑的任务（包括 AI 与扫描）都会让它排队或失败。
        let blocked_reason = match self.tasks.active().first() {
            Some(task) => Some(format!("「{}」正在进行，完成后才能撤销", task.label)),
            None if entries.is_empty() => Some("还没有可撤销的操作".to_owned()),
            None => None,
        };
        Ok(HistoryPage {
            entries,
            max_steps: self.settings.snapshot().history.max_steps,
            pending_purge_count,
            pending_purge_bytes,
            can_undo: blocked_reason.is_none(),
            blocked_reason,
        })
    }

    pub fn undo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let (database, _) = self.query_source()?;
        let (status, failed, count) = database.with_reader(|connection| {
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
            let count: i64 = connection.query_row(
                "SELECT count(*) FROM operations
                 WHERE id >= ?1 AND status IN ('applied', 'undoing')",
                [target_id],
                |row| row.get(0),
            )?;
            Ok((status, failed, count))
        })?;
        match status.as_deref() {
            Some("applied") => {}
            Some("undoFailed") => {
                return Err(AppError::message(
                    "这条记录上次撤销失败，需要先处理失败原因",
                ));
            }
            Some(_) => return Err(AppError::message("只能撤销已经完成的操作")),
            None => return Err(AppError::message("操作记录不存在")),
        }
        if failed > 0 {
            return Err(AppError::message(
                "之后有一条记录上次撤销失败，不能继续回退；请先处理失败原因或清空记录",
            ));
        }
        if count <= 0 {
            return Err(AppError::message("没有可撤销的记录"));
        }
        self.start_undo_task(target_id, count as usize)
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

    /// 设置里的最大步数变化后立即裁剪；失败只记日志，不影响设置保存。
    pub fn after_settings_changed(&self) {
        self.after_history_write();
    }

    /// 图库可用后的收尾：先续做中断的撤销（撤销不可排队，要先占 write lane），再按上限裁剪。
    fn after_library_open(&self) {
        self.schedule_recovery();
        self.after_history_write();
    }

    fn after_history_write(&self) {
        let Some((database, _, root)) = self.library_handles() else {
            return;
        };
        let max_steps = self.settings.snapshot().history.max_steps;
        match history::trim(&database, max_steps) {
            Ok(evicted) => {
                if evicted
                    .iter()
                    .any(|operation| operation.kind.keeps_staged_files())
                {
                    if let Err(error) = history::cleanup_undo_dirs(&database, &root) {
                        log::warn!("清理撤销暂存目录失败：{error}");
                    }
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
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("读取未完成撤销记录失败：{error}"),
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
        let (database, root) = self.query_source()?;
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
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker("undo-history".to_owned(), task.clone(), move || {
                    let _guard: LaneGuard = guard;
                    task.set_phase(TaskPhase::Running);
                    let result = history::run_undo(&database, &root, target_id, task.clone());
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

    pub fn tasks_snapshot(&self) -> Vec<TaskSnapshot> {
        self.tasks.list()
    }

    pub fn similarity_groups_result(&self, task_id: &str) -> Option<SimilarityGroupsResult> {
        self.ai.groups_result(task_id)
    }
}

fn conflict_error(active: &[TaskSummary]) -> AppError {
    let label = active
        .first()
        .map(|task| task.label.as_str())
        .unwrap_or("其他任务");
    AppError::message(format!("「{label}」正在进行，请等待完成或取消"))
}

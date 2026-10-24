use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
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
    library: Mutex<Option<LibrarySession>>,
    import_job: Mutex<Option<Arc<ImportJob>>>,
}

/// 缩略图重建连续失败达到该数量即停止，避免把整库都记成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;

impl AppState {
    pub fn new(settings: SettingsStore) -> Self {
        Self {
            settings,
            ai: AiRuntime::new(),
            tasks: TaskBus::new(),
            library: Mutex::new(None),
            import_job: Mutex::new(None),
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
        Ok(info)
    }

    pub fn open_library(&self, path: &Path) -> AppResult<LibraryInfo> {
        self.ensure_no_activity()?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let session = service::open(path, read_connections)?;
        let info = session.info.clone();
        self.settings.set_library_path(Some(info.path.clone()))?;
        self.ai.library_opened(
            info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            PathBuf::from(&info.path),
        );
        *self.library.lock() = Some(session);
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
        folder_service::create(&database, &root, path)
    }

    pub fn rename_folder(&self, folder_id: i64, name: &str) -> AppResult<Vec<FolderInfo>> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        folder_service::rename(&database, &root, folder_id, name)
    }

    pub fn move_folder(&self, folder_id: i64, parent_id: i64) -> AppResult<Vec<FolderInfo>> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        folder_service::move_to(&database, &root, folder_id, parent_id)
    }

    pub fn delete_folder(&self, folder_id: i64) -> AppResult<Vec<FolderInfo>> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        folder_service::delete(&database, &root, folder_id)
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
        metadata::update_score(&database, image_id, score)
    }

    pub fn set_images_score(&self, image_ids: &[i64], score: i64) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        metadata::set_scores(&database, image_ids, score)
    }

    pub fn update_image_description(&self, image_id: i64, description: &str) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        metadata::update_description(&database, image_id, description)
    }

    pub fn set_image_tags(&self, image_id: i64, tag_names: &[String]) -> AppResult<Vec<TagRef>> {
        let (database, _) = self.query_source()?;
        metadata::set_image_tags(&database, image_id, tag_names)
    }

    pub fn add_tags(&self, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        metadata::add_tags(&database, image_ids, tag_names)
    }

    pub fn remove_tags(&self, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        metadata::remove_tags(&database, image_ids, tag_names)
    }

    pub fn rename_tag(&self, tag_id: i64, name: &str) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        metadata::rename_tag(&database, tag_id, name)
    }

    pub fn delete_tag(&self, tag_id: i64) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        metadata::delete_tag(&database, tag_id)
    }

    pub fn move_images(
        &self,
        image_ids: &[i64],
        folder_path: &str,
        reporter: &dyn BatchReporter,
    ) -> AppResult<MoveReport> {
        let (database, root) = self.query_source()?;
        metadata::move_images(&database, &root, image_ids, folder_path, reporter)
    }

    pub fn trash_images(&self, image_ids: &[i64]) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        let changed = metadata::set_trashed(&database, image_ids, true)?;
        self.ai.invalidate_similarity();
        Ok(changed)
    }

    pub fn restore_images(&self, image_ids: &[i64]) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        let changed = metadata::set_trashed(&database, image_ids, false)?;
        self.ai.invalidate_similarity();
        Ok(changed)
    }

    pub fn delete_images(
        &self,
        image_ids: &[i64],
        reporter: &dyn BatchReporter,
    ) -> AppResult<DeleteReport> {
        let (database, root) = self.query_source()?;
        let report = metadata::delete_images(&database, &root, image_ids, reporter)?;
        self.ai.invalidate_similarity();
        Ok(report)
    }

    pub fn empty_trash(&self, reporter: &dyn BatchReporter) -> AppResult<DeleteReport> {
        let (database, root) = self.query_source()?;
        let report = metadata::empty_trash(&database, &root, reporter)?;
        self.ai.invalidate_similarity();
        Ok(report)
    }

    pub fn collections(&self) -> AppResult<Vec<CollectionInfo>> {
        let (database, _) = self.query_source()?;
        collections::list(&database)
    }

    pub fn create_collection(&self, name: &str) -> AppResult<CollectionInfo> {
        let (database, _) = self.query_source()?;
        collections::create(&database, name)
    }

    pub fn rename_collection(&self, collection_id: i64, name: &str) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        collections::rename(&database, collection_id, name)
    }

    pub fn delete_collection(&self, collection_id: i64) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        collections::delete(&database, collection_id)
    }

    pub fn reorder_collections(&self, collection_ids: &[i64]) -> AppResult<()> {
        let (database, _) = self.query_source()?;
        collections::reorder(&database, collection_ids)
    }

    pub fn add_images_to_collection(
        &self,
        collection_id: i64,
        image_ids: &[i64],
    ) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        collections::add_images(&database, collection_id, image_ids)
    }

    pub fn remove_images_from_collection(
        &self,
        collection_id: i64,
        image_ids: &[i64],
    ) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        collections::remove_images(&database, collection_id, image_ids)
    }

    pub fn combine_collections(
        &self,
        mode: CombineMode,
        collection_ids: &[i64],
        name: &str,
    ) -> AppResult<CollectionInfo> {
        let (database, _) = self.query_source()?;
        collections::combine(&database, mode, collection_ids, name)
    }

    pub fn image_pose(&self, image_id: i64) -> AppResult<Option<PoseData>> {
        self.ai.read_pose(image_id)
    }

    pub(crate) fn begin_edit(&self) -> AppResult<LaneGuard> {
        let guard = self.begin_maintenance()?;
        self.ai.invalidate_similarity();
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

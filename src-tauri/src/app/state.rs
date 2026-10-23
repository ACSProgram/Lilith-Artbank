use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;

use crate::{
    app::{
        bench::ThumbBench,
        error::{AppError, AppResult},
        settings::{LibraryEntry, Settings, SettingsStore},
    },
    import::{
        ImportRequest, ImportSource,
        job::{ImportJob, ImportSnapshot},
    },
    library::{
        collections::{self, CollectionInfo, CombineMode},
        content,
        db::LibraryDb,
        manifest::{LibraryInspection, inspect},
        metadata::{self, DeleteReport, ImageDetails, MoveReport, TagRef},
        query::{self, FilterCounts, FolderInfo, ImagePage, LibraryQuery, TagInfo},
        service::{self, LibraryInfo, LibrarySession},
        thumbs::ThumbPack,
        viewer::{self, PoseData},
    },
};

pub(crate) struct MaintenanceGuard(Arc<AtomicBool>);

impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct AppState {
    pub settings: SettingsStore,
    library: Mutex<Option<LibrarySession>>,
    thumb_bench: Mutex<Option<Arc<ThumbBench>>>,
    import_job: Mutex<Option<Arc<ImportJob>>>,
    maintenance: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(settings: SettingsStore) -> Self {
        Self {
            settings,
            library: Mutex::new(None),
            thumb_bench: Mutex::new(None),
            import_job: Mutex::new(None),
            maintenance: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn current(&self) -> Option<LibraryInfo> {
        self.library
            .lock()
            .as_ref()
            .map(|session| session.info.clone())
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

    pub fn thumb_bench(&self) -> Option<std::sync::Arc<ThumbBench>> {
        self.thumb_bench.lock().clone()
    }

    pub fn set_thumb_bench(&self, bench: Option<std::sync::Arc<ThumbBench>>) {
        *self.thumb_bench.lock() = bench;
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

    pub fn import_running(&self) -> bool {
        self.import_job
            .lock()
            .as_ref()
            .is_some_and(|job| job.is_running())
    }

    pub fn start_import(
        &self,
        app: tauri::AppHandle,
        request: ImportRequest,
    ) -> AppResult<ImportSnapshot> {
        self.ensure_no_activity()?;
        let mut request = request.normalized()?;
        request.worker_threads = self.settings.snapshot().performance.worker_threads;

        let (database, thumbs, root) = {
            let library = self.library.lock();
            let session = library
                .as_ref()
                .ok_or_else(|| AppError::message("尚未打开图库"))?;
            (
                session.db.clone(),
                session.thumbs.clone(),
                PathBuf::from(&session.info.path),
            )
        };

        let mut slot = self.import_job.lock();
        if slot.as_ref().is_some_and(|job| job.is_running()) {
            return Err(AppError::message("已有导入任务在运行"));
        }
        let job = std::sync::Arc::new(ImportJob::new(uuid::Uuid::new_v4().to_string()));
        *slot = Some(job.clone());
        drop(slot);

        crate::import::start(
            app,
            ImportSource {
                database,
                thumbs,
                root,
            },
            job.clone(),
            request,
        );
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
        if self.maintenance.load(Ordering::Acquire) {
            return Err(AppError::message("文件操作进行中，请等待完成"));
        }
        if self.import_running() {
            return Err(AppError::message("导入进行中，请等待完成或取消导入"));
        }
        Ok(())
    }

    fn begin_maintenance(&self) -> AppResult<MaintenanceGuard> {
        self.ensure_no_activity()?;
        if self.maintenance.swap(true, Ordering::AcqRel) {
            return Err(AppError::message("已有文件操作在运行"));
        }
        Ok(MaintenanceGuard(self.maintenance.clone()))
    }

    pub fn create_library(&self, path: &Path, name: &str) -> AppResult<LibraryInfo> {
        self.ensure_no_activity()?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let (session, manifest) = service::create(path, name, read_connections)?;
        let info = session.info.clone();
        self.settings.upsert_library(LibraryEntry {
            id: manifest.library_id.clone(),
            name: manifest.name.clone(),
            path: info.path.clone(),
            added_at: service::now_ms(),
        })?;
        self.settings.set_last_library(Some(manifest.library_id))?;
        *self.library.lock() = Some(session);
        Ok(info)
    }

    pub fn open_library(&self, path: &Path) -> AppResult<LibraryInfo> {
        self.ensure_no_activity()?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let session = service::open(path, read_connections)?;
        let info = session.info.clone();
        self.settings.upsert_library(LibraryEntry {
            id: info.id.clone(),
            name: info.name.clone(),
            path: info.path.clone(),
            added_at: service::now_ms(),
        })?;
        self.settings.set_last_library(Some(info.id.clone()))?;
        *self.library.lock() = Some(session);
        Ok(info)
    }

    pub fn close_library(&self) -> AppResult<()> {
        self.ensure_no_activity()?;
        *self.library.lock() = None;
        self.settings.set_last_library(None)?;
        Ok(())
    }

    pub fn remove_library(&self, library_id: &str) -> AppResult<Settings> {
        self.ensure_no_activity()?;
        let is_current = self
            .current()
            .map(|info| info.id == library_id)
            .unwrap_or(false);
        if is_current {
            *self.library.lock() = None;
        }
        self.settings.remove_library(library_id)
    }

    pub fn rename_library(&self, library_id: &str, name: &str) -> AppResult<Settings> {
        let settings = self.settings.rename_library(library_id, name)?;
        let mut guard = self.library.lock();
        if let Some(session) = guard.as_mut()
            && session.info.id == library_id
        {
            let trimmed = name.trim();
            session.info.name = trimmed.to_owned();
            let path = std::path::Path::new(&session.info.path);
            if let Err(error) = service::rename_manifest(path, trimmed) {
                log::warn!("图库清单重命名失败：{error}");
            }
        }
        Ok(settings)
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

    pub fn move_images(&self, image_ids: &[i64], folder_path: &str) -> AppResult<MoveReport> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        metadata::move_images(&database, &root, image_ids, folder_path)
    }

    pub fn trash_images(&self, image_ids: &[i64]) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        metadata::set_trashed(&database, image_ids, true)
    }

    pub fn restore_images(&self, image_ids: &[i64]) -> AppResult<u64> {
        let (database, _) = self.query_source()?;
        metadata::set_trashed(&database, image_ids, false)
    }

    pub fn delete_images(&self, image_ids: &[i64]) -> AppResult<DeleteReport> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        metadata::delete_images(&database, &root, image_ids)
    }

    pub fn empty_trash(&self) -> AppResult<DeleteReport> {
        let _guard = self.begin_maintenance()?;
        let (database, root) = self.query_source()?;
        metadata::empty_trash(&database, &root)
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
        let (database, _) = self.query_source()?;
        viewer::pose(&database, image_id)
    }

    pub(crate) fn begin_edit(&self) -> AppResult<MaintenanceGuard> {
        self.begin_maintenance()
    }

    pub(crate) fn edit_handles(&self) -> AppResult<(Arc<LibraryDb>, Arc<ThumbPack>, PathBuf)> {
        self.library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))
    }

    pub(crate) fn copy_source(&self, image_id: i64) -> AppResult<PathBuf> {
        let (database, root) = self.query_source()?;
        content::original_path(&database, &root, image_id)
    }
}

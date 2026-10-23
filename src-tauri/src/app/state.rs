use std::path::{Path, PathBuf};

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
        db::LibraryDb,
        manifest::{LibraryInspection, inspect},
        service::{self, LibraryInfo, LibrarySession},
        thumbs::ThumbPack,
    },
};

pub struct AppState {
    pub settings: SettingsStore,
    library: Mutex<Option<LibrarySession>>,
    thumb_bench: Mutex<Option<std::sync::Arc<ThumbBench>>>,
    import_job: Mutex<Option<std::sync::Arc<ImportJob>>>,
}

impl AppState {
    pub fn new(settings: SettingsStore) -> Self {
        Self {
            settings,
            library: Mutex::new(None),
            thumb_bench: Mutex::new(None),
            import_job: Mutex::new(None),
        }
    }

    pub fn current(&self) -> Option<LibraryInfo> {
        self.library
            .lock()
            .as_ref()
            .map(|session| session.info.clone())
    }

    #[allow(dead_code)]
    pub fn current_database(&self) -> Option<std::sync::Arc<crate::library::db::LibraryDb>> {
        self.library
            .lock()
            .as_ref()
            .map(|session| session.db.clone())
    }

    #[allow(dead_code)]
    pub fn require_database(&self) -> AppResult<std::sync::Arc<crate::library::db::LibraryDb>> {
        self.current_database()
            .ok_or_else(|| AppError::message("尚未打开图库"))
    }

    pub fn current_thumb_source(
        &self,
    ) -> Option<(std::sync::Arc<LibraryDb>, std::sync::Arc<ThumbPack>)> {
        self.library
            .lock()
            .as_ref()
            .map(|session| (session.db.clone(), session.thumbs.clone()))
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

    fn ensure_no_import(&self) -> AppResult<()> {
        if self.import_running() {
            return Err(AppError::message("导入进行中，请等待完成或取消导入"));
        }
        Ok(())
    }

    pub fn create_library(&self, path: &Path, name: &str) -> AppResult<LibraryInfo> {
        self.ensure_no_import()?;
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
        self.ensure_no_import()?;
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
        self.ensure_no_import()?;
        *self.library.lock() = None;
        self.settings.set_last_library(None)?;
        Ok(())
    }

    pub fn remove_library(&self, library_id: &str) -> AppResult<Settings> {
        self.ensure_no_import()?;
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
}

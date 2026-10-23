use std::path::Path;

use parking_lot::Mutex;

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::{LibraryEntry, Settings, SettingsStore},
    },
    library::{
        manifest::{LibraryInspection, inspect},
        service::{self, LibraryInfo, LibrarySession},
    },
};

pub struct AppState {
    pub settings: SettingsStore,
    library: Mutex<Option<LibrarySession>>,
}

impl AppState {
    pub fn new(settings: SettingsStore) -> Self {
        Self {
            settings,
            library: Mutex::new(None),
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

    pub fn create_library(&self, path: &Path, name: &str) -> AppResult<LibraryInfo> {
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
        *self.library.lock() = None;
        self.settings.set_last_library(None)?;
        Ok(())
    }

    pub fn remove_library(&self, library_id: &str) -> AppResult<Settings> {
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
        if let Some(session) = guard.as_mut() {
            if session.info.id == library_id {
                let trimmed = name.trim();
                session.info.name = trimmed.to_owned();
                let path = std::path::Path::new(&session.info.path);
                if let Err(error) = service::rename_manifest(path, trimmed) {
                    log::warn!("图库清单重命名失败：{error}");
                }
            }
        }
        Ok(settings)
    }

    pub fn inspect_library(&self, path: &Path) -> LibraryInspection {
        inspect(path)
    }
}

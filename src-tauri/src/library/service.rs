use std::{path::Path, sync::Arc};

use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        manifest::{LibraryInspection, LibraryManifest, inspect},
        paths::LibraryPaths,
        schema::{self, SCHEMA_VERSION},
        temp,
        thumbs::ThumbPack,
    },
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryInfo {
    pub id: String,
    pub name: String,
    pub path: String,
    pub image_count: u64,
    pub trash_count: u64,
    pub temp_count: u64,
    pub schema_version: u32,
    pub opened_at: i64,
}

pub struct LibrarySession {
    pub info: LibraryInfo,
    pub db: Arc<LibraryDb>,
    pub thumbs: Arc<ThumbPack>,
}

/// 图库三档计数；打开图库不再同步计算，前端按需单独拉取。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryCounts {
    pub image_count: u64,
    pub trash_count: u64,
    pub temp_count: u64,
}

pub fn create(
    path: &Path,
    name: &str,
    read_connections: u32,
) -> AppResult<(LibrarySession, LibraryManifest)> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(AppError::message("图库名称必须为 1-120 个字符"));
    }
    if !path.is_absolute() {
        return Err(AppError::message("图库路径必须是绝对路径"));
    }
    match inspect(path) {
        LibraryInspection::Library { .. } => {
            return Err(AppError::message("该目录已经是一个图库"));
        }
        LibraryInspection::Invalid { reason } => {
            return Err(AppError::message(format!("该目录无法作为图库：{reason}")));
        }
        LibraryInspection::Empty => {}
    }

    std::fs::create_dir_all(path)?;
    let paths = LibraryPaths::new(path);
    paths.ensure_runtime_dirs()?;
    let thumbs = Arc::new(ThumbPack::open(&paths.thumbs)?);
    let manifest = LibraryManifest::new(name, now_ms());
    LibraryManifest::write_atomic(&paths.manifest, &manifest)?;

    let database = match LibraryDb::create(&paths.database, read_connections) {
        Ok(database) => database,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&paths.data);
            return Err(error);
        }
    };
    if let Err(error) = database.with_writer(|connection| schema::initialize(connection, &manifest))
    {
        let _ = std::fs::remove_dir_all(&paths.data);
        return Err(error);
    }
    temp::ensure(&database, path)?;

    let info = LibraryInfo {
        id: manifest.library_id.clone(),
        name: manifest.name.clone(),
        path: path.to_string_lossy().into_owned(),
        image_count: 0,
        trash_count: 0,
        temp_count: 0,
        schema_version: SCHEMA_VERSION,
        opened_at: now_ms(),
    };
    Ok((
        LibrarySession {
            info,
            db: Arc::new(database),
            thumbs,
        },
        manifest,
    ))
}

pub fn open(path: &Path, read_connections: u32) -> AppResult<LibrarySession> {
    if !path.is_absolute() {
        return Err(AppError::message("图库路径必须是绝对路径"));
    }
    let manifest = match inspect(path) {
        LibraryInspection::Library { manifest } => manifest,
        LibraryInspection::Empty => {
            return Err(AppError::message("目录不是图库，请先创建图库"));
        }
        LibraryInspection::Invalid { reason } => {
            return Err(AppError::message(format!("图库无法打开：{reason}")));
        }
    };
    let paths = LibraryPaths::new(path);
    paths.ensure_runtime_dirs()?;
    clean_temporary_directory(path)?;
    let thumbs = Arc::new(ThumbPack::open(&paths.thumbs)?);
    let database = LibraryDb::open(&paths.database, read_connections)?;
    database.with_writer(|connection| schema::ensure(connection, &manifest))?;
    temp::ensure(&database, path)?;

    Ok(LibrarySession {
        info: LibraryInfo {
            id: manifest.library_id.clone(),
            name: manifest.name.clone(),
            path: path.to_string_lossy().into_owned(),
            image_count: 0,
            trash_count: 0,
            temp_count: 0,
            schema_version: SCHEMA_VERSION,
            opened_at: now_ms(),
        },
        db: Arc::new(database),
        thumbs,
    })
}

pub fn rename_manifest(path: &Path, name: &str) -> AppResult<()> {
    let paths = LibraryPaths::new(path);
    let mut manifest = LibraryManifest::read(&paths.manifest)?;
    manifest.name = name.trim().to_owned();
    LibraryManifest::write_atomic(&paths.manifest, &manifest)
}

pub fn clean_temporary_directory(path: &Path) -> AppResult<()> {
    let paths = LibraryPaths::new(path);
    if !paths.temp.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&paths.temp)? {
        let entry = entry?;
        let target = entry.path();
        if target.is_dir() {
            let _ = std::fs::remove_dir_all(&target);
        } else {
            let _ = std::fs::remove_file(&target);
        }
    }
    Ok(())
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_opens_library() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("图库");
        let (session, manifest) = create(&root, "参考", 2).unwrap();
        assert_eq!(session.info.name, "参考");
        assert_eq!(manifest.format, crate::library::paths::LIBRARY_FORMAT);
        assert!(root.join(".artbank").join("library.db").is_file());
        assert!(matches!(inspect(&root), LibraryInspection::Library { .. }));

        let reopened = open(&root, 2).unwrap();
        assert_eq!(reopened.info.id, manifest.library_id);
        assert_eq!(reopened.info.image_count, 0);
    }

    #[test]
    fn refuses_to_create_twice() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("图库");
        create(&root, "参考", 1).unwrap();
        assert!(create(&root, "另一个", 1).is_err());
    }

    #[test]
    fn refuses_to_open_plain_directory() {
        let temp = tempfile::tempdir().unwrap();
        assert!(open(temp.path(), 1).is_err());
    }
}

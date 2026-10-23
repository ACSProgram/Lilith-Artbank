use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::paths::{LIBRARY_FORMAT, LIBRARY_FORMAT_VERSION, LibraryPaths},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryManifest {
    pub format: String,
    pub format_version: u32,
    pub library_id: String,
    pub name: String,
    pub created_at: i64,
}

impl LibraryManifest {
    pub fn new(name: &str, created_at: i64) -> Self {
        Self {
            format: LIBRARY_FORMAT.into(),
            format_version: LIBRARY_FORMAT_VERSION,
            library_id: uuid::Uuid::new_v4().to_string(),
            name: name.trim().to_owned(),
            created_at,
        }
    }

    pub fn read(path: &Path) -> AppResult<Self> {
        let bytes = std::fs::read(path)?;
        let manifest: Self = serde_json::from_slice(&bytes)?;
        if manifest.format != LIBRARY_FORMAT {
            return Err(AppError::message("目录不是 Lilith Artbank 图库"));
        }
        if manifest.format_version != LIBRARY_FORMAT_VERSION {
            return Err(AppError::message(format!(
                "图库格式版本 {} 不受支持（当前 {}）",
                manifest.format_version, LIBRARY_FORMAT_VERSION
            )));
        }
        if manifest.library_id.trim().is_empty() {
            return Err(AppError::message("图库清单缺少 libraryId"));
        }
        Ok(manifest)
    }

    pub fn write_atomic(path: &Path, manifest: &Self) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(manifest)?;
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LibraryInspection {
    Library { manifest: LibraryManifest },
    Empty,
    Invalid { reason: String },
}

pub fn inspect(path: &Path) -> LibraryInspection {
    if path.is_file() {
        return LibraryInspection::Invalid {
            reason: "目标是一个文件，不是目录".into(),
        };
    }
    let paths = LibraryPaths::new(path);
    if !paths.data.exists() {
        return LibraryInspection::Empty;
    }
    let manifest = match LibraryManifest::read(&paths.manifest) {
        Ok(manifest) => manifest,
        Err(error) => {
            return LibraryInspection::Invalid {
                reason: error.to_string(),
            };
        }
    };
    if !paths.database.is_file() {
        return LibraryInspection::Invalid {
            reason: "图库数据库丢失".into(),
        };
    }
    LibraryInspection::Library { manifest }
}

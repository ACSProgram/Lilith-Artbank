// 这些路径助手在 Stage 1/3 才会接入；保留完整布局，避免各阶段重复定义。
#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};

use crate::app::error::{AppError, AppResult};

pub const DATA_DIR: &str = ".artbank";
pub const DATABASE_FILE: &str = "library.db";
pub const MANIFEST_FILE: &str = "library.json";
pub const THUMBS_DIR: &str = "thumbs";
pub const FEATURES_DIR: &str = "features";
pub const TEMP_DIR: &str = "temp";
pub const RECYCLE_DIR: &str = "recycle";
pub const OUTPUT_DIR: &str = "output";
pub const LIBRARY_FORMAT: &str = "lilith-artbank";
pub const LIBRARY_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct LibraryPaths {
    pub root: PathBuf,
    pub data: PathBuf,
    pub database: PathBuf,
    pub manifest: PathBuf,
    pub thumbs: PathBuf,
    pub features: PathBuf,
    pub temp: PathBuf,
    pub recycle: PathBuf,
    pub output: PathBuf,
}

impl LibraryPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let data = root.join(DATA_DIR);
        Self {
            database: data.join(DATABASE_FILE),
            manifest: data.join(MANIFEST_FILE),
            thumbs: data.join(THUMBS_DIR),
            features: data.join(FEATURES_DIR),
            temp: data.join(TEMP_DIR),
            recycle: root.join(RECYCLE_DIR),
            output: root.join(OUTPUT_DIR),
            data,
            root,
        }
    }

    pub fn ensure_runtime_dirs(&self) -> AppResult<()> {
        std::fs::create_dir_all(&self.data)?;
        std::fs::create_dir_all(&self.thumbs)?;
        std::fs::create_dir_all(&self.features)?;
        std::fs::create_dir_all(&self.temp)?;
        Ok(())
    }

    pub fn content_file(&self, folder: &str, file_name: &str) -> PathBuf {
        if folder.is_empty() {
            self.root.join(file_name)
        } else {
            self.root
                .join(folder.replace('/', std::path::MAIN_SEPARATOR_STR))
                .join(file_name)
        }
    }

    pub fn thumb_segment(&self, segment_id: u32) -> PathBuf {
        self.thumbs.join(format!("{segment_id:04}.pack"))
    }

    pub fn feature_file(&self, kind: &str) -> PathBuf {
        self.features.join(format!("{kind}.bin"))
    }
}

/// 图库内文件夹使用 `/` 分隔的相对路径；根目录用空字符串表示。
pub fn normalize_folder(value: &str) -> AppResult<String> {
    let raw = value.trim().replace('\\', "/");
    if raw.starts_with('/') || raw.contains(':') {
        return Err(AppError::message("文件夹必须是图库内的相对路径"));
    }
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.chars().count() > 512 {
        return Err(AppError::message("文件夹路径过长"));
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err(AppError::message("文件夹路径包含控制字符"));
    }
    let path = Path::new(trimmed);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(AppError::message("文件夹必须是图库内的相对路径"));
    }
    Ok(trimmed.to_owned())
}

pub fn normalize_content_extension(value: &str) -> AppResult<String> {
    let ext = value.trim().trim_start_matches('.').to_ascii_lowercase();
    if ext.is_empty() || ext.len() > 16 || !ext.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return Err(AppError::message("图片扩展名无效"));
    }
    Ok(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_folder_paths() {
        assert_eq!(normalize_folder("").unwrap(), "");
        assert_eq!(normalize_folder(" 人物\\速写 ").unwrap(), "人物/速写");
        assert!(normalize_folder("../外面").is_err());
        assert!(normalize_folder(r"C:\绝对").is_err());
        assert!(normalize_folder("/root").is_err());
    }
}

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use crate::{
    app::error::{AppError, AppResult},
    library::{db::LibraryDb, metadata, paths::LibraryPaths},
};

/// 查看器与编辑保存共用的原图行快照。
///
/// `phash` / `colors*` / `modified_at` 只用于编辑撤销的前置快照；查看器路径不读它们。
pub struct ContentRow {
    pub id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub folder_path: String,
    pub file_size: i64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub has_alpha: bool,
    pub phash: Vec<u8>,
    pub colors4: Vec<u8>,
    pub colors8: Vec<u8>,
    pub modified_at: i64,
    pub thumb_version: i64,
    pub deleted_at: Option<i64>,
}

impl ContentRow {
    pub fn pixel_count(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    pub fn path(&self, paths: &LibraryPaths) -> AppResult<PathBuf> {
        metadata::content_path(paths, &self.folder_path, &self.content_hash, &self.file_ext)
    }
}

/// 原图绝对路径；调用方不自己拼路径，避免绕过内容哈希与目录校验。
pub fn original_path(database: &LibraryDb, root: &Path, image_id: i64) -> AppResult<PathBuf> {
    load(database, image_id)?.path(&LibraryPaths::new(root))
}

pub fn load(database: &LibraryDb, image_id: i64) -> AppResult<ContentRow> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    database.with_reader(|connection| {
        connection
            .query_row(
                "SELECT images.id, images.content_hash, images.file_ext,
                        folders.path, images.file_size, images.width, images.height,
                        images.format, images.has_alpha, images.phash, images.colors4,
                        images.colors8, images.modified_at, images.thumb_version, images.deleted_at
                 FROM images
                 JOIN folders ON folders.id = images.folder_id
                 WHERE images.id = ?1",
                [image_id],
                |row| {
                    Ok(ContentRow {
                        id: row.get(0)?,
                        content_hash: row.get(1)?,
                        file_ext: row.get(2)?,
                        folder_path: row.get(3)?,
                        file_size: row.get(4)?,
                        width: row.get(5)?,
                        height: row.get(6)?,
                        format: row.get(7)?,
                        has_alpha: row.get::<_, i64>(8)? != 0,
                        phash: row.get(9)?,
                        colors4: row.get(10)?,
                        colors8: row.get(11)?,
                        modified_at: row.get(12)?,
                        thumb_version: row.get(13)?,
                        deleted_at: row.get(14)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| AppError::message("图片不存在"))
    })
}

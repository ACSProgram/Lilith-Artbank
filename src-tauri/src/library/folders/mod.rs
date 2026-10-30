use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::paths::ensure_within,
};

mod mutate;
mod trash;
mod tree;
mod undo;

#[cfg(test)]
mod tests;

pub use mutate::*;
pub use trash::*;
pub(crate) use undo::*;

pub(super) fn folder_id(connection: &Connection, path: &str) -> AppResult<Option<i64>> {
    connection
        .query_row(
            "SELECT id FROM folders WHERE path = ?1 COLLATE NOCASE",
            [path],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::from)
}

pub(super) fn folder_path(connection: &Connection, id: i64) -> AppResult<String> {
    if id <= 0 {
        return Err(AppError::message("文件夹 id 无效"));
    }
    connection
        .query_row("SELECT path FROM folders WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| AppError::message("文件夹不存在"))
}

/// 临时文件夹由生命周期管理，禁止重命名、移动、删除或参与同级排序。
/// 回收站文件夹与待释放文件夹也不参与正常树的写入。
pub(super) fn ensure_normal_folder(connection: &Connection, id: i64) -> AppResult<()> {
    let state: Option<(String, Option<i64>, Option<i64>)> = connection
        .query_row(
            "SELECT kind, trashed_at, purge_operation_id FROM folders WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((kind, trashed_at, purge_operation_id)) = state else {
        return Ok(());
    };
    if kind == "temp" {
        return Err(AppError::message("临时文件夹不能重命名、移动、删除或排序"));
    }
    if purge_operation_id.is_some() {
        return Err(AppError::message("文件夹正在释放，不能再修改"));
    }
    if trashed_at.is_some() {
        return Err(AppError::message("文件夹在回收站中，请先恢复或永久删除"));
    }
    Ok(())
}

/// 只有回收站里的普通文件夹能恢复或永久删除。
pub(super) fn ensure_trashed_folder(connection: &Connection, id: i64) -> AppResult<String> {
    let state: Option<(String, Option<i64>, Option<i64>, String)> = connection
        .query_row(
            "SELECT kind, trashed_at, purge_operation_id, path FROM folders WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((kind, trashed_at, purge_operation_id, path)) = state else {
        return Err(AppError::message("文件夹不存在"));
    };
    if kind == "temp" {
        return Err(AppError::message("临时文件夹不能恢复或永久删除"));
    }
    if purge_operation_id.is_some() {
        return Err(AppError::message("文件夹正在释放，请稍后再试"));
    }
    if trashed_at.is_none() {
        return Err(AppError::message("文件夹不在回收站中"));
    }
    Ok(path)
}

/// 目标路径已被占用时给出可操作的原因：回收站与待释放优先于普通重名。
pub(super) fn ensure_target_free(
    connection: &Connection,
    path: &str,
    self_id: i64,
    conflict: &str,
) -> AppResult<()> {
    let state: Option<(i64, Option<i64>, Option<i64>)> = connection
        .query_row(
            "SELECT id, trashed_at, purge_operation_id FROM folders
             WHERE path = ?1 COLLATE NOCASE",
            [path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((id, trashed_at, purge_operation_id)) = state else {
        return Ok(());
    };
    if id == self_id {
        return Ok(());
    }
    if purge_operation_id.is_some() {
        return Err(AppError::message("目标文件夹正在释放，请稍后再试"));
    }
    if trashed_at.is_some() {
        return Err(AppError::message(
            "目标路径的文件夹在回收站中，请先恢复或永久删除",
        ));
    }
    Err(AppError::message(conflict))
}

pub(super) fn ensure_inside_root(root: &Path, directory: &Path) -> AppResult<()> {
    ensure_within(root, directory, "文件夹位于图库目录之外")
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderSortKey {
    Name,
    Total,
}

impl FolderSortKey {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Total => "图片数",
        }
    }
}

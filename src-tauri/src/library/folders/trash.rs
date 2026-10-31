use std::path::Path;

use rusqlite::{
    Connection, OptionalExtension, Transaction, params, params_from_iter, types::Value,
};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        fs_journal::{self, FileJournal},
        history::{self, FolderRow, HistoryKind, NewOperation},
        paths::{
            INTERNAL_PATH_MAX, LibraryPaths, normalize_stored_folder, parent_path,
            subtree_like_pattern, trash_folder_path,
        },
        placeholders,
        query::{self, FolderChange},
        service::now_ms,
    },
};

use super::mutate::info_in;
use super::tree::ancestor_folder_ids;
use super::{ensure_normal_folder, ensure_trashed_folder};

type FolderUnitState = (String, Option<String>, Option<i64>, Option<i64>);

/// 整棵子树移入回收站：目录改名为仓库路径，原路径立即释放。
pub fn trash(db: &LibraryDb, root: &Path, folder_id_value: i64) -> AppResult<FolderChange> {
    trash_with_id(db, root, folder_id_value).map(|(change, _)| change)
}

pub fn trash_with_id(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path =
        db.with_reader(|connection| folder_display_path(connection, folder_id_value))?;
    let before = db.with_reader(|connection| info_in(connection, folder_id_value))?;
    let mut removed_ids = Vec::new();
    let mut image_count = 0_u64;
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderTrash,
            format!(
                "把文件夹「{label_path}」移入回收站（{} 张图片）",
                before.total_image_count
            ),
        ),
        |transaction, draft| {
            ensure_normal_folder(transaction, folder_id_value)?;
            let path: String = transaction
                .query_row(
                    "SELECT path FROM folders WHERE id = ?1",
                    [folder_id_value],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| AppError::message("文件夹不存在"))?;
            if path.is_empty() {
                return Err(AppError::message("不能删除图库根目录"));
            }
            let pattern = subtree_like_pattern(&path);
            let rows = normal_subtree_rows(transaction, &path, &pattern)?;
            if rows.is_empty() {
                return Err(AppError::message("文件夹不在正常树中"));
            }
            let targets: Vec<(i64, i64)> = {
                let mut statement = transaction.prepare(
                    "SELECT images.id, images.modified_at FROM images
                     JOIN folders ON folders.id = images.folder_id
                     WHERE (folders.path = ?1 OR folders.path LIKE ?2 ESCAPE '\\')
                       AND images.deleted_at IS NULL AND images.purge_operation_id IS NULL",
                )?;
                statement
                    .query_map(params![&path, &pattern], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let stamp = next_trash_stamp(transaction, root)?;
            let record_rows =
                apply_trash_unit(transaction, root, &rows, stamp, false, draft.journal())?;
            for chunk in targets.chunks(4_000) {
                let marks = placeholders(chunk.len());
                transaction.execute(
                    &format!(
                        "UPDATE images SET deleted_at = ?1, modified_at = ?1
                         WHERE id IN ({marks})
                           AND deleted_at IS NULL AND purge_operation_id IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(stamp))
                            .chain(chunk.iter().map(|(id, _)| Value::Integer(*id))),
                    ),
                )?;
            }
            let triples: Vec<(i64, i64, i64)> = targets
                .iter()
                .map(|(id, modified)| (*id, 0, *modified))
                .collect();
            draft.json(&record_rows)?;
            draft.triples(&triples);
            draft.affected(record_rows.len() as u64);
            removed_ids = record_rows.iter().map(|row| row.id).collect();
            image_count = targets.len() as u64;
            Ok(())
        },
    )?;
    let ancestors =
        db.with_reader(|connection| ancestor_folder_ids(connection, parent_path(&label_path)))?;
    Ok((
        FolderChange {
            folders: query::folder_infos(db, &ancestors)?,
            removed_ids,
            affected_images: image_count,
        },
        operation_id,
    ))
}

/// 恢复回收站文件夹：把整个删除单元搬回原路径。
///
/// 删除前就已经单独在回收站里的图片不跟着恢复；文件夹恢复后它们归到根行“直属图片”。
/// 上级文件夹若来自另一次删除，拒绝恢复；原路径被占用时也拒绝，不做自动合并或改名。
pub fn restore(db: &LibraryDb, root: &Path, folder_id_value: i64) -> AppResult<FolderChange> {
    restore_with_id(db, root, folder_id_value).map(|(change, _)| change)
}

pub fn restore_with_id(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path =
        db.with_reader(|connection| folder_display_path(connection, folder_id_value))?;
    let mut restored_images = 0_u64;
    let mut restored_path = label_path.clone();
    let mut restored_ids = Vec::new();
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderRestore,
            format!("恢复文件夹「{label_path}」"),
        ),
        |transaction, draft| {
            ensure_trashed_folder(transaction, folder_id_value)?;
            let stamp: i64 = transaction.query_row(
                "SELECT trashed_at FROM folders WHERE id = ?1",
                [folder_id_value],
                |row| row.get(0),
            )?;
            let rows = trash_unit_rows(transaction, stamp)?;
            if rows.is_empty() {
                return Err(AppError::message("文件夹不在回收站中"));
            }
            let legacy = rows.iter().all(|row| row.original_path.is_none());
            let root_row = unit_root(&rows)?.clone();
            let root_original = root_row
                .original_path
                .clone()
                .unwrap_or_else(|| root_row.path.clone());
            apply_restore_unit(transaction, root, &rows, legacy, true, draft.journal())?;

            let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
            let targets = restore_unit_images(transaction, &ids, stamp)?;
            let now = now_ms();
            for chunk in targets.chunks(4_000) {
                let marks = placeholders(chunk.len());
                transaction.execute(
                    &format!(
                        "UPDATE images SET deleted_at = NULL, modified_at = ?1
                         WHERE id IN ({marks}) AND purge_operation_id IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(now))
                            .chain(chunk.iter().map(|(id, _, _)| Value::Integer(*id))),
                    ),
                )?;
            }
            draft.json(&rows)?;
            draft.triples(&targets);
            draft.affected(rows.len() as u64);
            restored_ids = ids;
            restored_images = targets.len() as u64;
            restored_path = root_original;
            Ok(())
        },
    )?;
    let mut ids = restored_ids;
    ids.extend(
        db.with_reader(|connection| ancestor_folder_ids(connection, parent_path(&restored_path)))?,
    );
    ids.sort_unstable();
    ids.dedup();
    Ok((
        FolderChange {
            folders: query::folder_infos(db, &ids)?,
            removed_ids: Vec::new(),
            affected_images: restored_images,
        },
        operation_id,
    ))
}

/// 永久删除回收站文件夹：只写待释放标记，释放任务删完仓库内容后再清理文件夹行与空目录。
pub fn delete_permanently(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderChange> {
    delete_permanently_with_id(db, folder_id_value).map(|(change, _)| change)
}

pub fn delete_permanently_with_id(
    db: &LibraryDb,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path =
        db.with_reader(|connection| folder_display_path(connection, folder_id_value))?;
    let mut removed_ids = Vec::new();
    let mut image_count = 0_u64;
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderPurge,
            format!("永久删除文件夹「{label_path}」"),
        ),
        |transaction, draft| {
            let path = ensure_trashed_folder(transaction, folder_id_value)?;
            let pattern = subtree_like_pattern(&path);
            let ids: Vec<i64> = {
                let mut statement = transaction.prepare(
                    "SELECT id FROM folders
                     WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
                       AND trashed_at IS NOT NULL AND purge_operation_id IS NULL",
                )?;
                statement
                    .query_map(params![&path, &pattern], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if ids.is_empty() {
                return Err(AppError::message("文件夹已经在释放中"));
            }
            transaction.execute(
                "UPDATE folders SET purge_operation_id = ?1
                 WHERE (path = ?2 OR path LIKE ?3 ESCAPE '\\')
                   AND purge_operation_id IS NULL",
                params![draft.id(), &path, &pattern],
            )?;
            image_count = transaction.execute(
                "UPDATE images SET purge_operation_id = ?1
                 WHERE folder_id IN (
                     SELECT purge_folders.id FROM folders purge_folders
                     WHERE purge_folders.path = ?2 OR purge_folders.path LIKE ?3 ESCAPE '\\'
                 )
                   AND deleted_at IS NOT NULL AND purge_operation_id IS NULL",
                params![draft.id(), &path, &pattern],
            )? as u64;
            draft.affected(ids.len() as u64);
            removed_ids = ids;
            Ok(())
        },
    )?;
    Ok((
        FolderChange {
            folders: Vec::new(),
            removed_ids,
            affected_images: image_count,
        },
        operation_id,
    ))
}

/// 启动对账：让 `.artbank/trash` 里的删除单元与数据库一致。
///
/// 只处理仓库根目录存在的单元；无法判定时只记日志，不删数据。
pub(crate) fn reconcile_trash(db: &LibraryDb, root: &Path) -> AppResult<()> {
    let paths = LibraryPaths::new(root);
    if !paths.trash.is_dir() {
        return Ok(());
    }
    for stamp in std::fs::read_dir(&paths.trash)? {
        let stamp = stamp?;
        if !stamp.file_type()?.is_dir() {
            continue;
        }
        for unit in std::fs::read_dir(stamp.path())? {
            let unit = unit?;
            if !unit.file_type()?.is_dir() {
                continue;
            }
            let Some(root_id) = unit.file_name().to_string_lossy().parse::<i64>().ok() else {
                continue;
            };
            let state: Option<FolderUnitState> = db.with_reader(|connection| {
                Ok(connection
                    .query_row(
                        "SELECT path, original_path, trashed_at, purge_operation_id
                             FROM folders WHERE id = ?1",
                        [root_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?)
            })?;
            let Some((path, original_path, trashed_at, purge)) = state else {
                continue;
            };
            if purge.is_some() {
                continue;
            }
            match (trashed_at, original_path) {
                // 删除事务没有提交：数据库还是正常行，把仓库目录搬回原路径。
                (None, None) => {
                    let destination = paths.folder_dir(&path);
                    if !destination.exists() {
                        if !parent_exists(&destination) {
                            log::warn!("回收站对账：原路径的上级目录不存在，跳过 {path}");
                            continue;
                        }
                        match std::fs::rename(unit.path(), &destination) {
                            Ok(()) => log::info!("回收站对账：把未提交删除的目录搬回 {path}"),
                            Err(error) => {
                                log::warn!("回收站对账：搬回 {path} 失败：{error}");
                            }
                        }
                    } else {
                        log::warn!("回收站对账：原路径已被占用，保留仓库目录待处理：{path}");
                    }
                }
                // 已提交的删除由下面的数据库扫描补搬；这里只需跳过。
                (Some(_), Some(_)) => {}
                // v10 原地回收站行或状态不完整：不搬目录。
                _ => {}
            }
        }
    }

    // 仓库目录不存在时扫描目录发现不了，按数据库里的回收站行反向补齐。
    let pending = db.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT path, original_path FROM folders
             WHERE trashed_at IS NOT NULL AND original_path IS NOT NULL
               AND purge_operation_id IS NULL
             ORDER BY length(path)",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    for (stored, original) in pending {
        let warehouse = paths.folder_dir(&stored);
        if warehouse.exists() {
            continue;
        }
        let source = paths.folder_dir(&original);
        if !source.is_dir() {
            log::warn!("回收站对账：找不到目录「{original}」，保留数据库行待用户处理");
            continue;
        }
        if let Some(parent) = warehouse.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("回收站对账：创建仓库目录失败：{error}");
            continue;
        }
        match std::fs::rename(&source, &warehouse) {
            Ok(()) => log::info!("回收站对账：把「{original}」补搬入回收站仓库"),
            Err(error) => {
                log::warn!("回收站对账：补搬「{original}」失败：{error}");
            }
        }
    }

    // 恢复或释放搬空的删除单元时间戳目录直接删掉；非空时 remove_dir 自然失败。
    if let Ok(entries) = std::fs::read_dir(&paths.trash) {
        for entry in entries.flatten() {
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }
    Ok(())
}

fn parent_exists(path: &Path) -> bool {
    path.parent().map(Path::is_dir).unwrap_or(true)
}

/// 展示路径：回收站行优先用原路径，旧 v10 行回退到 `path`。
pub(super) fn folder_display_path(connection: &Connection, id: i64) -> AppResult<String> {
    connection
        .query_row(
            "SELECT coalesce(original_path, path) FROM folders WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::message("文件夹不存在"))
}

fn normal_subtree_rows(
    transaction: &Transaction<'_>,
    path: &str,
    pattern: &str,
) -> AppResult<Vec<FolderRow>> {
    let mut statement = transaction.prepare(
        "SELECT id, path, created_at, sort_index, original_path, trashed_at FROM folders
         WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
           AND trashed_at IS NULL AND purge_operation_id IS NULL
         ORDER BY path",
    )?;
    let rows = statement
        .query_map(params![path, pattern], |row| {
            Ok(FolderRow {
                id: row.get(0)?,
                path: row.get(1)?,
                created_at: row.get(2)?,
                sort_index: row.get(3)?,
                original_path: row.get(4)?,
                trashed_at: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn trash_unit_rows(
    transaction: &Transaction<'_>,
    stamp: i64,
) -> AppResult<Vec<FolderRow>> {
    let mut statement = transaction.prepare(
        "SELECT id, path, created_at, sort_index, original_path, trashed_at FROM folders
         WHERE trashed_at = ?1 AND purge_operation_id IS NULL
         ORDER BY path",
    )?;
    let rows = statement
        .query_map([stamp], |row| {
            Ok(FolderRow {
                id: row.get(0)?,
                path: row.get(1)?,
                created_at: row.get(2)?,
                sort_index: row.get(3)?,
                original_path: row.get(4)?,
                trashed_at: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(super) fn unit_root(rows: &[FolderRow]) -> AppResult<&FolderRow> {
    rows.iter()
        .min_by_key(|row| (row.path.matches('/').count(), row.id))
        .ok_or_else(|| AppError::message("文件夹不在回收站中"))
}

struct WarehouseRow {
    id: i64,
    original_path: String,
    new_path: String,
    created_at: i64,
    sort_index: i64,
}

/// 把删除单元搬入仓库：目录只改根目录名，数据库行改成仓库路径。
pub(super) fn apply_trash_unit(
    transaction: &Transaction<'_>,
    root: &Path,
    rows: &[FolderRow],
    stamp: i64,
    legacy: bool,
    journal: &mut FileJournal,
) -> AppResult<Vec<FolderRow>> {
    let root_row = unit_root(rows)?.clone();
    let source_root = root_row
        .original_path
        .clone()
        .unwrap_or_else(|| root_row.path.clone());
    let warehouse_root = trash_folder_path(stamp, root_row.id);
    normalize_stored_folder(&warehouse_root)?;

    let layout = warehouse_layout(rows, &source_root, &warehouse_root)?;
    if !legacy {
        let source = LibraryPaths::new(root).folder_dir(&source_root);
        let destination = LibraryPaths::new(root).folder_dir(&warehouse_root);
        if !source.is_dir() {
            return Err(AppError::message(format!(
                "文件夹目录不存在：{}",
                source.display()
            )));
        }
        if destination.exists() {
            return Err(AppError::message("回收站目标目录已存在，请重试"));
        }
        if let Some(parent) = destination.parent() {
            journal.create_dir_all(parent)?;
        }
        fs_journal::rename_path(&source, &destination, false)?;
        journal.renamed(&source, &destination);
    }

    let mut record = Vec::with_capacity(layout.len());
    for item in &layout {
        transaction.execute(
            "UPDATE folders SET path = ?1, original_path = ?2, trashed_at = ?3 WHERE id = ?4",
            params![
                if legacy {
                    &item.original_path
                } else {
                    &item.new_path
                },
                if legacy {
                    None
                } else {
                    Some(&item.original_path)
                },
                stamp,
                item.id
            ],
        )?;
        record.push(FolderRow {
            id: item.id,
            path: if legacy {
                item.original_path.clone()
            } else {
                item.new_path.clone()
            },
            created_at: item.created_at,
            sort_index: item.sort_index,
            original_path: if legacy {
                None
            } else {
                Some(item.original_path.clone())
            },
            trashed_at: Some(stamp),
        });
    }
    Ok(record)
}

fn warehouse_layout(
    rows: &[FolderRow],
    source_root: &str,
    warehouse_root: &str,
) -> AppResult<Vec<WarehouseRow>> {
    rows.iter()
        .map(|row| {
            let original_path = row
                .original_path
                .clone()
                .unwrap_or_else(|| row.path.clone());
            let suffix = original_path
                .strip_prefix(source_root)
                .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
                .ok_or_else(|| AppError::message("文件夹状态与记录不符，无法移入回收站"))?;
            let new_path = format!("{warehouse_root}{suffix}");
            if new_path.chars().count() > INTERNAL_PATH_MAX {
                return Err(AppError::message("文件夹路径过长，请先缩短路径"));
            }
            normalize_stored_folder(&new_path)?;
            Ok(WarehouseRow {
                id: row.id,
                original_path,
                new_path,
                created_at: row.created_at,
                sort_index: row.sort_index,
            })
        })
        .collect()
}

/// 把删除单元搬回原路径；`legacy` 行保持原地语义，只改数据库状态。
pub(super) fn apply_restore_unit(
    transaction: &Transaction<'_>,
    root: &Path,
    rows: &[FolderRow],
    legacy: bool,
    check_conflicts: bool,
    journal: &mut FileJournal,
) -> AppResult<()> {
    let root_row = unit_root(rows)?.clone();
    let root_original = root_row
        .original_path
        .clone()
        .unwrap_or_else(|| root_row.path.clone());
    if check_conflicts {
        if !legacy {
            ensure_original_path_free(transaction, &root_original, root_row.id)?;
        }
        ensure_unit_parent_available(transaction, &root_original, root_row.id)?;
    }
    if !legacy {
        // 记录里的仓库路径可能已被上一次撤销改成新的时间戳；以数据库当前状态为准。
        let current: Option<(String, Option<i64>)> = transaction
            .query_row(
                "SELECT path, trashed_at FROM folders WHERE id = ?1",
                [root_row.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((current_path, Some(_))) = current else {
            return Err(AppError::message("文件夹状态与记录不符，无法恢复"));
        };
        let paths = LibraryPaths::new(root);
        let source = paths.folder_dir(&current_path);
        let destination = paths.folder_dir(&root_original);
        if !source.is_dir() {
            return Err(AppError::message("回收站目录不存在，无法恢复"));
        }
        if destination.exists() {
            return Err(AppError::message(
                "原位置已经有同名文件夹，请先重命名或移走占用的文件夹",
            ));
        }
        fs_journal::rename_path(&source, &destination, false)?;
        journal.renamed(&source, &destination);
    }

    for row in rows {
        let original = row
            .original_path
            .clone()
            .unwrap_or_else(|| row.path.clone());
        transaction.execute(
            "UPDATE folders SET path = ?1, original_path = NULL, trashed_at = NULL WHERE id = ?2",
            params![original, row.id],
        )?;
    }
    Ok(())
}

fn ensure_original_path_free(
    connection: &Connection,
    original_path: &str,
    unit_root_id: i64,
) -> AppResult<()> {
    let conflict: Option<i64> = connection
        .query_row(
            "SELECT id FROM folders WHERE path = ?1 COLLATE NOCASE AND id <> ?2",
            params![original_path, unit_root_id],
            |row| row.get(0),
        )
        .optional()?;
    if conflict.is_some() {
        return Err(AppError::message(
            "原位置已经有同名文件夹，请先重命名或移走占用的文件夹",
        ));
    }
    Ok(())
}

/// 原路径的上级若是另一个删除单元，先恢复上级；不做自动合并或改到别处。
fn ensure_unit_parent_available(
    connection: &Connection,
    original_path: &str,
    unit_root_id: i64,
) -> AppResult<()> {
    let parent = parent_path(original_path);
    if parent.is_empty() {
        return Ok(());
    }
    let mut statement = connection.prepare(
        "SELECT id, trashed_at, purge_operation_id FROM folders
         WHERE path = ?1 COLLATE NOCASE OR original_path = ?1 COLLATE NOCASE",
    )?;
    let states = statement
        .query_map([parent], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if states
        .iter()
        .any(|(id, trashed, purge)| *id != unit_root_id && trashed.is_none() && purge.is_none())
    {
        return Ok(());
    }
    if states
        .iter()
        .any(|(id, _, purge)| *id != unit_root_id && purge.is_some())
    {
        return Err(AppError::message("上级文件夹正在释放，请稍后再试"));
    }
    if states
        .iter()
        .any(|(id, trashed, _)| *id != unit_root_id && trashed.is_some())
    {
        return Err(AppError::message(format!(
            "上级文件夹「{parent}」仍在回收站中，请先恢复它"
        )));
    }
    Ok(())
}

fn restore_unit_images(
    transaction: &Transaction<'_>,
    folder_ids: &[i64],
    stamp: i64,
) -> AppResult<Vec<(i64, i64, i64)>> {
    let mut targets = Vec::new();
    for chunk in folder_ids.chunks(4_000) {
        let marks = placeholders(chunk.len());
        let mut statement = transaction.prepare(&format!(
            "SELECT images.id, images.deleted_at, images.modified_at FROM images
             WHERE images.deleted_at = ?1 AND folder_id IN ({marks})
               AND images.purge_operation_id IS NULL"
        ))?;
        targets.extend(
            statement
                .query_map(
                    params_from_iter(
                        std::iter::once(Value::Integer(stamp))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    Ok(targets)
}

/// 文件夹删除事件的时间戳：在所有删除单元之间唯一。
pub(super) fn next_trash_stamp(transaction: &Transaction<'_>, root: &Path) -> AppResult<i64> {
    let latest: Option<i64> = transaction.query_row(
        "SELECT MAX(stamp) FROM (
             SELECT MAX(trashed_at) AS stamp FROM folders
             UNION ALL
             SELECT MAX(deleted_at) FROM images
         )",
        [],
        |row| row.get(0),
    )?;
    let paths = LibraryPaths::new(root);
    let mut stamp = now_ms().max(latest.unwrap_or(0).saturating_add(1));
    while paths.trash.join(stamp.to_string()).exists() {
        stamp = stamp.saturating_add(1);
    }
    Ok(stamp)
}

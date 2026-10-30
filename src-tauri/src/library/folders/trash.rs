use rusqlite::{
    Connection, OptionalExtension, Transaction, params, params_from_iter, types::Value,
};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        history::{self, FolderRow, HistoryKind, NewOperation},
        paths::{parent_path, subtree_like_pattern},
        placeholders,
        query::{self, FolderChange},
        service::now_ms,
    },
};

use super::mutate::info_in;
use super::tree::ancestor_folder_ids;
use super::{ensure_normal_folder, ensure_trashed_folder, folder_id, folder_path};

/// 整棵子树移入回收站：保留原路径与文件，只写 `trashed_at` / `deleted_at`。
pub fn trash(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderChange> {
    trash_with_id(db, folder_id_value).map(|(change, _)| change)
}

pub fn trash_with_id(
    db: &LibraryDb,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path = db.with_reader(|connection| folder_path(connection, folder_id_value))?;
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
            let stamp = next_trash_stamp(transaction, &path)?;
            let rows: Vec<FolderRow> = {
                let mut statement = transaction.prepare(
                    "SELECT id, path, created_at, sort_index FROM folders
                     WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
                       AND trashed_at IS NULL AND purge_operation_id IS NULL
                     ORDER BY path",
                )?;
                statement
                    .query_map(params![&path, &pattern], |row| {
                        Ok(FolderRow {
                            id: row.get(0)?,
                            path: row.get(1)?,
                            created_at: row.get(2)?,
                            sort_index: row.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if rows.is_empty() {
                return Err(AppError::message("文件夹不在正常树中"));
            }
            transaction.execute(
                "UPDATE folders SET trashed_at = ?1
                 WHERE (path = ?2 OR path LIKE ?3 ESCAPE '\\')
                   AND trashed_at IS NULL AND purge_operation_id IS NULL",
                params![stamp, &path, &pattern],
            )?;
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
            draft.json(&rows)?;
            draft.triples(&triples);
            draft.affected(rows.len() as u64);
            removed_ids = rows.iter().map(|row| row.id).collect();
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

/// 恢复回收站文件夹：恢复这次删除的整个单元（同一时间戳的最高一级文件夹及其子树）。
///
/// 删除前就已经单独在回收站里的图片不跟着恢复；文件夹恢复后它们归到根行“直属图片”。
/// 上级文件夹若来自更早的一次删除，拒绝恢复，避免把两个删除单元拆开。
pub fn restore(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderChange> {
    restore_with_id(db, folder_id_value).map(|(change, _)| change)
}

pub fn restore_with_id(
    db: &LibraryDb,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path = db.with_reader(|connection| folder_path(connection, folder_id_value))?;
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
            let requested = ensure_trashed_folder(transaction, folder_id_value)?;
            let stamp: i64 = transaction.query_row(
                "SELECT trashed_at FROM folders WHERE id = ?1",
                [folder_id_value],
                |row| row.get(0),
            )?;
            // 同一次删除可能包含上级：选中子树里的文件夹时，恢复整个删除单元。
            let mut path = requested;
            let mut current = parent_path(&path);
            while !current.is_empty() {
                let Some(id) = folder_id(transaction, current)? else {
                    break;
                };
                let (ancestor_path, ancestor_trashed, ancestor_purge): (
                    String,
                    Option<i64>,
                    Option<i64>,
                ) = transaction.query_row(
                    "SELECT path, trashed_at, purge_operation_id FROM folders WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                if ancestor_purge.is_some() {
                    return Err(AppError::message("上级文件夹正在释放，请稍后再试"));
                }
                match ancestor_trashed {
                    Some(ancestor_stamp) if ancestor_stamp == stamp => {
                        path = ancestor_path;
                        current = parent_path(&path);
                    }
                    Some(_) => {
                        return Err(AppError::message(format!(
                            "上级文件夹「{ancestor_path}」仍在回收站中，请先恢复它"
                        )));
                    }
                    None => break,
                }
            }
            let pattern = subtree_like_pattern(&path);
            let rows: Vec<FolderRow> = {
                let mut statement = transaction.prepare(
                    "SELECT id, path, created_at, sort_index FROM folders
                     WHERE (path = ?1 OR path LIKE ?2 ESCAPE '\\')
                       AND trashed_at = ?3 AND purge_operation_id IS NULL
                     ORDER BY path",
                )?;
                statement
                    .query_map(params![&path, &pattern, stamp], |row| {
                        Ok(FolderRow {
                            id: row.get(0)?,
                            path: row.get(1)?,
                            created_at: row.get(2)?,
                            sort_index: row.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if rows.is_empty() {
                return Err(AppError::message("文件夹不在回收站中"));
            }
            restored_ids = rows.iter().map(|row| row.id).collect();
            for chunk in rows.chunks(4_000) {
                let marks = placeholders(chunk.len());
                let ids: Vec<i64> = chunk.iter().map(|row| row.id).collect();
                transaction.execute(
                    &format!("UPDATE folders SET trashed_at = NULL WHERE id IN ({marks})"),
                    params_from_iter(ids.iter()),
                )?;
            }
            let targets: Vec<(i64, i64, i64)> = {
                let mut statement = transaction.prepare(
                    "SELECT images.id, images.deleted_at, images.modified_at FROM images
                     JOIN folders ON folders.id = images.folder_id
                     WHERE (folders.path = ?1 OR folders.path LIKE ?2 ESCAPE '\\')
                       AND images.deleted_at = ?3 AND images.purge_operation_id IS NULL",
                )?;
                statement
                    .query_map(params![&path, &pattern, stamp], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
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
            restored_images = targets.len() as u64;
            restored_path = path;
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

/// 永久删除回收站文件夹：只写待释放标记，释放任务删完图片后再清理文件夹行与空目录。
pub fn delete_permanently(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderChange> {
    delete_permanently_with_id(db, folder_id_value).map(|(change, _)| change)
}

pub fn delete_permanently_with_id(
    db: &LibraryDb,
    folder_id_value: i64,
) -> AppResult<(FolderChange, Option<i64>)> {
    let label_path = db.with_reader(|connection| folder_path(connection, folder_id_value))?;
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

/// 文件夹删除事件的时间戳：必须大于子树里已有的回收站时间，恢复时才能按等值判断“这次一起进来的图片”。
fn next_trash_stamp(connection: &Connection, path: &str) -> AppResult<i64> {
    let latest: Option<i64> = connection.query_row(
        "SELECT MAX(stamp) FROM (
             SELECT MAX(images.deleted_at) AS stamp FROM images
              JOIN folders ON folders.id = images.folder_id
              WHERE folders.path = ?1 OR folders.path LIKE ?2 ESCAPE '\\'
             UNION ALL
             SELECT MAX(folders.trashed_at) FROM folders
              WHERE folders.path = ?1 OR folders.path LIKE ?2 ESCAPE '\\'
         )",
        params![path, subtree_like_pattern(path)],
        |row| row.get(0),
    )?;
    Ok(now_ms().max(latest.unwrap_or(0).saturating_add(1)))
}

pub(super) fn restore_folder_rows(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
) -> AppResult<()> {
    for chunk in rows.chunks(4_000) {
        let marks = placeholders(chunk.len());
        transaction.execute(
            &format!("UPDATE folders SET trashed_at = NULL WHERE id IN ({marks})"),
            params_from_iter(chunk.iter().map(|row| row.id)),
        )?;
    }
    Ok(())
}

pub(super) fn trash_folder_rows(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    stamp: i64,
) -> AppResult<()> {
    for chunk in rows.chunks(4_000) {
        let marks = placeholders(chunk.len());
        transaction.execute(
            &format!(
                "UPDATE folders SET trashed_at = ?1
                 WHERE id IN ({marks}) AND trashed_at IS NULL"
            ),
            params_from_iter(
                std::iter::once(Value::Integer(stamp))
                    .chain(chunk.iter().map(|row| Value::Integer(row.id))),
            ),
        )?;
    }
    Ok(())
}

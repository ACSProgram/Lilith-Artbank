use rusqlite::{TransactionBehavior, params};

use crate::{
    app::error::{AppError, AppResult},
    library::db::LibraryDb,
};

use super::super::{
    ChunkFormat, HistoryKind, ImageState, RedoRecord,
    chunks::{decode_triples, load_chunks},
    replay::{append_redo, load_redo_records},
};

/// 回收站/删除类：撤销时把当前状态写进 redo chunk，重做时原样写回。
pub(crate) fn undo_visibility(
    database: &LibraryDb,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let chunks = load_chunks(&transaction, operation_id)?;
        match kind {
            HistoryKind::Trash | HistoryKind::Restore => {
                for chunk in chunks
                    .iter()
                    .filter(|chunk| chunk.format == ChunkFormat::Triples)
                {
                    for batch in decode_triples(&chunk.data)?.chunks(400) {
                        let marks = crate::library::placeholders(batch.len());
                        let mut statement = transaction.prepare(&format!(
                            "SELECT id, deleted_at, modified_at FROM images
                             WHERE id IN ({marks}) AND purge_operation_id IS NULL"
                        ))?;
                        let current = statement
                            .query_map(
                                rusqlite::params_from_iter(batch.iter().map(|(id, _, _)| id)),
                                |row| {
                                    Ok((
                                        row.get::<_, i64>(0)?,
                                        row.get::<_, Option<i64>>(1)?,
                                        row.get::<_, i64>(2)?,
                                    ))
                                },
                            )?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        drop(statement);

                        let mut redo = Vec::new();
                        for (image_id, old_deleted_at, old_modified_at) in batch {
                            let old_deleted_at = if *old_deleted_at == 0 {
                                None
                            } else {
                                Some(*old_deleted_at)
                            };
                            if let Some((_, deleted_at, modified_at)) =
                                current.iter().find(|(id, _, _)| id == image_id)
                                && (*deleted_at != old_deleted_at
                                    || *modified_at != *old_modified_at)
                            {
                                redo.push(ImageState {
                                    image_id: *image_id,
                                    deleted_at: *deleted_at,
                                    modified_at: *modified_at,
                                });
                            }
                            // 待释放图片由更晚的记录负责恢复；这里不把它变回正常状态。
                            transaction.execute(
                                "UPDATE images
                                 SET deleted_at = ?1, modified_at = ?2
                                 WHERE id = ?3 AND purge_operation_id IS NULL",
                                params![old_deleted_at, old_modified_at, image_id],
                            )?;
                        }
                        if !redo.is_empty() {
                            append_redo(
                                &transaction,
                                operation_id,
                                &RedoRecord::ImageStates { items: redo },
                            )?;
                        }
                    }
                }
            }
            _ => {
                let ids: Vec<i64> = {
                    let mut statement = transaction.prepare(
                        "SELECT id FROM images WHERE purge_operation_id = ?1 ORDER BY id",
                    )?;
                    statement
                        .query_map([operation_id], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                let folder_ids: Vec<i64> = {
                    let mut statement = transaction.prepare(
                        "SELECT id FROM folders WHERE purge_operation_id = ?1 ORDER BY id",
                    )?;
                    statement
                        .query_map([operation_id], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                if !ids.is_empty() {
                    append_redo(&transaction, operation_id, &RedoRecord::PurgeIds { ids })?;
                }
                if !folder_ids.is_empty() {
                    append_redo(
                        &transaction,
                        operation_id,
                        &RedoRecord::PurgeFolderIds { ids: folder_ids },
                    )?;
                }
                transaction.execute(
                    "UPDATE images SET purge_operation_id = NULL
                     WHERE purge_operation_id = ?1",
                    [operation_id],
                )?;
                transaction.execute(
                    "UPDATE folders SET purge_operation_id = NULL
                     WHERE purge_operation_id = ?1",
                    [operation_id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    })
}

pub(crate) fn redo_visibility(
    database: &LibraryDb,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let records = load_redo_records(&transaction, operation_id)?;
        match kind {
            HistoryKind::Trash | HistoryKind::Restore => {
                let mut applied = false;
                for record in records {
                    if let RedoRecord::ImageStates { items } = record {
                        for item in items {
                            transaction.execute(
                                "UPDATE images
                                 SET deleted_at = ?1, modified_at = ?2
                                 WHERE id = ?3",
                                params![item.deleted_at, item.modified_at, item.image_id],
                            )?;
                            applied = true;
                        }
                    }
                }
                if !applied {
                    return Err(AppError::message("这条记录缺少重做数据"));
                }
            }
            _ => {
                let mut applied = false;
                for record in records {
                    match record {
                        RedoRecord::PurgeIds { ids } => {
                            for id in ids {
                                transaction.execute(
                                    "UPDATE images SET purge_operation_id = ?1
                                     WHERE id = ?2 AND deleted_at IS NOT NULL
                                       AND purge_operation_id IS NULL",
                                    params![operation_id, id],
                                )?;
                                applied = true;
                            }
                        }
                        RedoRecord::PurgeFolderIds { ids } => {
                            for id in ids {
                                transaction.execute(
                                    "UPDATE folders SET purge_operation_id = ?1
                                     WHERE id = ?2 AND trashed_at IS NOT NULL
                                       AND purge_operation_id IS NULL",
                                    params![operation_id, id],
                                )?;
                                applied = true;
                            }
                        }
                        _ => {}
                    }
                }
                if !applied {
                    return Err(AppError::message("这条记录缺少重做数据"));
                }
            }
        }
        transaction.commit()?;
        Ok(())
    })
}

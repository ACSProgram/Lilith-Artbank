use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::de::DeserializeOwned;

use crate::{
    app::error::{AppError, AppResult},
    library::db::LibraryDb,
};

use super::super::{
    ChunkFormat, CollectionItemsRecord, CollectionNameState, CollectionOrderState,
    CollectionRenameRecord, CollectionReorderRecord, CollectionSnapshot, HistoryKind, RedoRecord,
    chunks::{decode_json, load_chunks},
    replay::{append_redo, load_redo_records},
};

/// 集合的当前完整状态；`collections` 的删除记录与撤销/重做共用这一份查询。
pub(crate) fn collection_snapshot(
    connection: &Connection,
    collection_id: i64,
) -> AppResult<Option<CollectionSnapshot>> {
    let row: Option<(i64, String, i64, i64, i64)> = connection
        .query_row(
            "SELECT id, name, sort_order, created_at, updated_at
             FROM collections WHERE id = ?1",
            [collection_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((id, name, sort_order, created_at, updated_at)) = row else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT image_id, position FROM collection_items
         WHERE collection_id = ?1 ORDER BY position, image_id",
    )?;
    let items = statement
        .query_map([collection_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(CollectionSnapshot {
        id,
        name,
        sort_order,
        created_at,
        updated_at,
        items,
    }))
}

fn insert_collection_snapshot(
    transaction: &Transaction<'_>,
    snapshot: &CollectionSnapshot,
) -> AppResult<()> {
    transaction.execute(
        "INSERT INTO collections(id, name, sort_order, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name,
             sort_order = excluded.sort_order,
             created_at = excluded.created_at,
             updated_at = excluded.updated_at",
        params![
            snapshot.id,
            snapshot.name,
            snapshot.sort_order,
            snapshot.created_at,
            snapshot.updated_at
        ],
    )?;
    for (image_id, position) in &snapshot.items {
        transaction.execute(
            "INSERT INTO collection_items(collection_id, image_id, position)
             SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM images WHERE id = ?2)
             ON CONFLICT(collection_id, image_id) DO UPDATE SET position = excluded.position",
            params![snapshot.id, image_id, position],
        )?;
    }
    Ok(())
}

fn load_collection_snapshot(
    connection: &Connection,
    operation_id: i64,
) -> AppResult<CollectionSnapshot> {
    let chunks = load_chunks(connection, operation_id)?;
    for chunk in chunks {
        if chunk.format == ChunkFormat::Json {
            return decode_json(&chunk.data);
        }
    }
    Err(AppError::message("这条记录缺少撤销数据"))
}

fn load_collection_snapshots(
    connection: &Connection,
    operation_id: i64,
) -> AppResult<Vec<CollectionSnapshot>> {
    let chunks = load_chunks(connection, operation_id)?;
    for chunk in chunks {
        if chunk.format == ChunkFormat::Json {
            return decode_json(&chunk.data);
        }
    }
    Err(AppError::message("这条记录缺少撤销数据"))
}

pub(super) fn collection_items(
    connection: &Connection,
    collection_id: i64,
    image_ids: &[i64],
) -> AppResult<Vec<(i64, i64)>> {
    let mut items = Vec::new();
    for chunk in image_ids.chunks(400) {
        let marks = crate::library::placeholders(chunk.len());
        let mut statement = connection.prepare(&format!(
            "SELECT image_id, position FROM collection_items
             WHERE collection_id = ?1 AND image_id IN ({marks})"
        ))?;
        items.extend(
            statement
                .query_map(
                    rusqlite::params_from_iter(
                        std::iter::once(rusqlite::types::Value::Integer(collection_id))
                            .chain(chunk.iter().map(|id| rusqlite::types::Value::Integer(*id))),
                    ),
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    items.sort_unstable();
    Ok(items)
}

/// 按 id 分批删除集合里的条目；撤销“加入”与重做“移出”共用。
fn delete_collection_items(
    transaction: &Transaction<'_>,
    collection_id: i64,
    image_ids: &[i64],
) -> AppResult<()> {
    for chunk in image_ids.chunks(400) {
        let marks = crate::library::placeholders(chunk.len());
        transaction.execute(
            &format!(
                "DELETE FROM collection_items
                 WHERE collection_id = ?1 AND image_id IN ({marks})"
            ),
            rusqlite::params_from_iter(
                std::iter::once(rusqlite::types::Value::Integer(collection_id))
                    .chain(chunk.iter().map(|id| rusqlite::types::Value::Integer(*id))),
            ),
        )?;
    }
    Ok(())
}

pub(crate) fn undo_collection(
    database: &LibraryDb,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match kind {
            HistoryKind::CollectionCreate | HistoryKind::CollectionCombine => {
                let snapshot = load_collection_snapshot(&transaction, operation_id)?;
                let live = collection_snapshot(&transaction, snapshot.id)?;
                if let Some(live) = live {
                    append_redo(
                        &transaction,
                        operation_id,
                        &RedoRecord::CollectionRestores { items: vec![live] },
                    )?;
                }
                transaction.execute("DELETE FROM collections WHERE id = ?1", [snapshot.id])?;
            }
            HistoryKind::SimilarityGroups => {
                let snapshots = load_collection_snapshots(&transaction, operation_id)?;
                let mut live = Vec::new();
                for snapshot in &snapshots {
                    if let Some(snapshot) = collection_snapshot(&transaction, snapshot.id)? {
                        live.push(snapshot);
                    }
                }
                if !live.is_empty() {
                    append_redo(
                        &transaction,
                        operation_id,
                        &RedoRecord::CollectionRestores { items: live },
                    )?;
                }
                for snapshot in &snapshots {
                    transaction.execute("DELETE FROM collections WHERE id = ?1", [snapshot.id])?;
                }
            }
            HistoryKind::CollectionRename => {
                let record: CollectionRenameRecord =
                    load_collection_snapshot_json(&transaction, operation_id)?;
                let current: Option<(String, i64)> = transaction
                    .query_row(
                        "SELECT name, updated_at FROM collections WHERE id = ?1",
                        [record.id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((name, updated_at)) = current else {
                    return Err(AppError::message("集合已不存在，无法撤销重命名"));
                };
                if name == record.old_name && updated_at == record.old_updated_at {
                    return Ok(());
                }
                append_redo(
                    &transaction,
                    operation_id,
                    &RedoRecord::CollectionNames {
                        items: vec![CollectionNameState {
                            id: record.id,
                            name,
                            updated_at,
                        }],
                    },
                )?;
                transaction.execute(
                    "UPDATE collections SET name = ?1, updated_at = ?2 WHERE id = ?3",
                    params![record.old_name, record.old_updated_at, record.id],
                )?;
            }
            HistoryKind::CollectionDelete => {
                let snapshot = load_collection_snapshot(&transaction, operation_id)?;
                append_redo(
                    &transaction,
                    operation_id,
                    &RedoRecord::CollectionDeletions {
                        ids: vec![snapshot.id],
                    },
                )?;
                insert_collection_snapshot(&transaction, &snapshot)?;
            }
            HistoryKind::CollectionReorder => {
                let record: CollectionReorderRecord =
                    load_collection_snapshot_json(&transaction, operation_id)?;
                let mut current = Vec::new();
                for row in &record.rows {
                    let state: Option<(i64, i64)> = transaction
                        .query_row(
                            "SELECT sort_order, updated_at FROM collections WHERE id = ?1",
                            [row.id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    if let Some((sort_order, updated_at)) = state {
                        current.push(CollectionOrderState {
                            id: row.id,
                            sort_order,
                            updated_at,
                        });
                    }
                }
                let changed = current.iter().any(|state| {
                    record
                        .rows
                        .iter()
                        .find(|row| row.id == state.id)
                        .is_some_and(|row| {
                            row.sort_order != state.sort_order || row.updated_at != state.updated_at
                        })
                });
                if changed {
                    append_redo(
                        &transaction,
                        operation_id,
                        &RedoRecord::CollectionOrders { rows: current },
                    )?;
                    for row in &record.rows {
                        transaction.execute(
                            "UPDATE collections SET sort_order = ?1, updated_at = ?2 WHERE id = ?3",
                            params![row.sort_order, row.updated_at, row.id],
                        )?;
                    }
                }
            }
            HistoryKind::CollectionAdd | HistoryKind::CollectionRemove => {
                let record: CollectionItemsRecord =
                    load_collection_snapshot_json(&transaction, operation_id)?;
                let image_ids: Vec<i64> = record.items.iter().map(|(id, _)| *id).collect();
                let updated_at: Option<i64> = transaction
                    .query_row(
                        "SELECT updated_at FROM collections WHERE id = ?1",
                        [record.collection_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(updated_at) = updated_at else {
                    return Err(AppError::message("集合已不存在，无法撤销"));
                };
                if kind == HistoryKind::CollectionAdd {
                    let present = collection_items(&transaction, record.collection_id, &image_ids)?;
                    if !present.is_empty() {
                        append_redo(
                            &transaction,
                            operation_id,
                            &RedoRecord::CollectionAdds {
                                collection_id: record.collection_id,
                                items: present.clone(),
                                updated_at,
                            },
                        )?;
                        let ids: Vec<i64> = present.iter().map(|(id, _)| *id).collect();
                        delete_collection_items(&transaction, record.collection_id, &ids)?;
                    }
                } else {
                    let mut restored = Vec::new();
                    for (image_id, position) in &record.items {
                        let changed = transaction.execute(
                            "INSERT INTO collection_items(collection_id, image_id, position)
                             SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM images WHERE id = ?2)
                             ON CONFLICT(collection_id, image_id) DO NOTHING",
                            params![record.collection_id, image_id, position],
                        )?;
                        if changed > 0 {
                            restored.push((*image_id, *position));
                        }
                    }
                    if !restored.is_empty() {
                        append_redo(
                            &transaction,
                            operation_id,
                            &RedoRecord::CollectionRemovals {
                                collection_id: record.collection_id,
                                items: restored,
                                updated_at,
                            },
                        )?;
                    }
                }
                transaction.execute(
                    "UPDATE collections SET updated_at = ?1 WHERE id = ?2",
                    params![record.old_updated_at, record.collection_id],
                )?;
            }
            _ => {}
        }
        transaction.commit()?;
        Ok(())
    })
}

fn load_collection_snapshot_json<T: DeserializeOwned>(
    connection: &Connection,
    operation_id: i64,
) -> AppResult<T> {
    let chunks = load_chunks(connection, operation_id)?;
    for chunk in chunks {
        if chunk.format == ChunkFormat::Json {
            return decode_json(&chunk.data);
        }
    }
    Err(AppError::message("这条记录缺少撤销数据"))
}

pub(crate) fn redo_collection(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut applied = false;
        for record in load_redo_records(&transaction, operation_id)? {
            match record {
                RedoRecord::CollectionRestores { items } => {
                    for snapshot in &items {
                        insert_collection_snapshot(&transaction, snapshot)?;
                    }
                    applied = true;
                }
                RedoRecord::CollectionDeletions { ids } => {
                    for id in ids {
                        transaction.execute("DELETE FROM collections WHERE id = ?1", [id])?;
                    }
                    applied = true;
                }
                RedoRecord::CollectionNames { items } => {
                    for item in items {
                        transaction.execute(
                            "UPDATE collections SET name = ?1, updated_at = ?2 WHERE id = ?3",
                            params![item.name, item.updated_at, item.id],
                        )?;
                    }
                    applied = true;
                }
                RedoRecord::CollectionOrders { rows } => {
                    for row in rows {
                        transaction.execute(
                            "UPDATE collections SET sort_order = ?1, updated_at = ?2 WHERE id = ?3",
                            params![row.sort_order, row.updated_at, row.id],
                        )?;
                    }
                    applied = true;
                }
                RedoRecord::CollectionAdds {
                    collection_id,
                    items,
                    updated_at,
                } => {
                    for (image_id, position) in &items {
                        transaction.execute(
                            "INSERT INTO collection_items(collection_id, image_id, position)
                             SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM images WHERE id = ?2)
                             ON CONFLICT(collection_id, image_id)
                             DO UPDATE SET position = excluded.position",
                            params![collection_id, image_id, position],
                        )?;
                    }
                    transaction.execute(
                        "UPDATE collections SET updated_at = ?1 WHERE id = ?2",
                        params![updated_at, collection_id],
                    )?;
                    applied = true;
                }
                RedoRecord::CollectionRemovals {
                    collection_id,
                    items,
                    updated_at,
                } => {
                    let ids: Vec<i64> = items.iter().map(|(id, _)| *id).collect();
                    delete_collection_items(&transaction, collection_id, &ids)?;
                    transaction.execute(
                        "UPDATE collections SET updated_at = ?1 WHERE id = ?2",
                        params![updated_at, collection_id],
                    )?;
                    applied = true;
                }
                _ => {}
            }
        }
        if !applied {
            return Err(AppError::message("这条记录缺少重做数据"));
        }
        transaction.commit()?;
        Ok(())
    })
}

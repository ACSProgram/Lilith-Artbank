use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{
    app::error::{AppError, AppResult},
    library::db::LibraryDb,
};

use super::super::{
    ChunkFormat, DescriptionRecord, RedoRecord, ScoreState,
    chunks::{decode_json, decode_triples, load_chunks},
    replay::{append_redo, load_redo_records},
};

/// 孤儿清理：把文件从 `recycle/orphans/` 搬回原位；原路径已被占用时跳过并计数。
pub(crate) fn undo_orphan_move(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pairs: Vec<(String, String)> = Vec::new();
        for chunk in load_chunks(&transaction, operation_id)? {
            if chunk.format == ChunkFormat::Json {
                pairs.extend(decode_json::<Vec<(String, String)>>(&chunk.data)?);
            }
        }
        if pairs.is_empty() {
            return Err(AppError::message("这条记录缺少撤销数据"));
        }
        let reverse: Vec<(String, String)> = pairs
            .iter()
            .map(|(source, destination)| (destination.clone(), source.clone()))
            .collect();
        let skipped = crate::library::maintenance_actions::move_pairs(&reverse);
        if !skipped.is_empty() {
            log::warn!("撤销孤儿清理时跳过 {} 个文件：{skipped:?}", skipped.len());
        }
        append_redo(&transaction, operation_id, &RedoRecord::PathMoves { pairs })?;
        transaction.commit()?;
        Ok(())
    })
}

pub(crate) fn redo_orphan_move(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut applied = false;
        for record in load_redo_records(&transaction, operation_id)? {
            if let RedoRecord::PathMoves { pairs } = record {
                let skipped = crate::library::maintenance_actions::move_pairs(&pairs);
                if !skipped.is_empty() {
                    log::warn!("重做孤儿清理时跳过 {} 个文件：{skipped:?}", skipped.len());
                }
                applied = true;
            }
        }
        if !applied {
            return Err(AppError::message("这条记录缺少重做数据"));
        }
        transaction.commit()?;
        Ok(())
    })
}

/// 评分：恢复旧评分与修改时间；撤销前的新值写进 redo。
pub(crate) fn undo_score(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut redo = Vec::new();
        for chunk in load_chunks(&transaction, operation_id)? {
            if chunk.format != ChunkFormat::Triples {
                continue;
            }
            for (image_id, old_score, old_modified_at) in decode_triples(&chunk.data)? {
                let current: Option<(i64, i64)> = transaction
                    .query_row(
                        "SELECT score, modified_at FROM images WHERE id = ?1",
                        [image_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((score, modified_at)) = current else {
                    continue;
                };
                if score == old_score && modified_at == old_modified_at {
                    continue;
                }
                redo.push(ScoreState {
                    image_id,
                    score,
                    modified_at,
                });
                transaction.execute(
                    "UPDATE images SET score = ?1, modified_at = ?2 WHERE id = ?3",
                    params![old_score, old_modified_at, image_id],
                )?;
            }
        }
        if !redo.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::ScoreStates { items: redo },
            )?;
        }
        transaction.commit()?;
        Ok(())
    })
}

pub(crate) fn redo_score(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut applied = false;
        for record in load_redo_records(&transaction, operation_id)? {
            if let RedoRecord::ScoreStates { items } = record {
                for item in items {
                    transaction.execute(
                        "UPDATE images SET score = ?1, modified_at = ?2 WHERE id = ?3",
                        params![item.score, item.modified_at, item.image_id],
                    )?;
                    applied = true;
                }
            }
        }
        if !applied {
            return Err(AppError::message("这条记录缺少重做数据"));
        }
        transaction.commit()?;
        Ok(())
    })
}

/// 描述（手工与 AI）：恢复旧文本、来源、模型与修改时间。
pub(crate) fn undo_description(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut redo = Vec::new();
        for chunk in load_chunks(&transaction, operation_id)? {
            if chunk.format != ChunkFormat::Json {
                continue;
            }
            let old: DescriptionRecord = decode_json(&chunk.data)?;
            let current: Option<(String, String, String, i64)> = transaction
                .query_row(
                    "SELECT description, description_source, description_model, modified_at
                     FROM images WHERE id = ?1",
                    [old.image_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((description, source, model, modified_at)) = current else {
                continue;
            };
            if description == old.description
                && source == old.description_source
                && model == old.description_model
                && modified_at == old.modified_at
            {
                continue;
            }
            redo.push(DescriptionRecord {
                image_id: old.image_id,
                description,
                description_source: source,
                description_model: model,
                modified_at,
            });
            transaction.execute(
                "UPDATE images
                 SET description = ?1, description_source = ?2, description_model = ?3,
                     modified_at = ?4
                 WHERE id = ?5",
                params![
                    old.description,
                    old.description_source,
                    old.description_model,
                    old.modified_at,
                    old.image_id
                ],
            )?;
        }
        if !redo.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::DescriptionStates { items: redo },
            )?;
        }
        transaction.commit()?;
        Ok(())
    })
}

pub(crate) fn redo_description(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut applied = false;
        for record in load_redo_records(&transaction, operation_id)? {
            if let RedoRecord::DescriptionStates { items } = record {
                for item in items {
                    transaction.execute(
                        "UPDATE images
                         SET description = ?1, description_source = ?2, description_model = ?3,
                             modified_at = ?4
                         WHERE id = ?5",
                        params![
                            item.description,
                            item.description_source,
                            item.description_model,
                            item.modified_at,
                            item.image_id
                        ],
                    )?;
                    applied = true;
                }
            }
        }
        if !applied {
            return Err(AppError::message("这条记录缺少重做数据"));
        }
        transaction.commit()?;
        Ok(())
    })
}

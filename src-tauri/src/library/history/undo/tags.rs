use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    app::error::{AppError, AppResult},
    library::db::LibraryDb,
};

use super::super::{
    ChunkFormat, HistoryKind, RedoRecord, TagRecord, TagRefRecord, TagSetState,
    chunks::load_chunks,
    replay::{append_redo, load_redo_records},
};

fn load_tag_records(connection: &Connection, operation_id: i64) -> AppResult<Vec<TagRecord>> {
    let mut records = Vec::new();
    for chunk in load_chunks(connection, operation_id)? {
        if chunk.format == ChunkFormat::Json {
            records.push(serde_json::from_slice(&chunk.data)?);
        }
    }
    Ok(records)
}

fn insert_tag_pair(transaction: &Transaction<'_>, image_id: i64, tag_id: i64) -> AppResult<bool> {
    let changed = transaction.execute(
        "INSERT INTO image_tags(image_id, tag_id)
         SELECT ?1, ?2
         WHERE EXISTS (SELECT 1 FROM images WHERE id = ?1)
           AND EXISTS (SELECT 1 FROM tags WHERE id = ?2)
         ON CONFLICT(image_id, tag_id) DO NOTHING",
        params![image_id, tag_id],
    )?;
    Ok(changed > 0)
}

/// 只在标签已没有任何图片引用时删除；返回真正删掉的行，供重做时重建。
fn delete_unreferenced_tags(
    transaction: &Transaction<'_>,
    candidates: &[TagRefRecord],
) -> AppResult<Vec<TagRefRecord>> {
    let mut deleted = Vec::new();
    let mut seen = HashSet::new();
    for tag in candidates {
        if !seen.insert(tag.id) {
            continue;
        }
        let referenced: i64 = transaction.query_row(
            "SELECT count(*) FROM image_tags WHERE tag_id = ?1",
            [tag.id],
            |row| row.get(0),
        )?;
        if referenced == 0 && transaction.execute("DELETE FROM tags WHERE id = ?1", [tag.id])? > 0 {
            deleted.push(tag.clone());
        }
    }
    Ok(deleted)
}

fn ensure_tag_row(transaction: &Transaction<'_>, tag: &TagRefRecord) -> AppResult<()> {
    let changed = transaction.execute(
        "UPDATE tags SET name = ?1 WHERE id = ?2",
        params![tag.name, tag.id],
    )?;
    if changed == 0 {
        transaction.execute(
            "INSERT INTO tags(id, name) VALUES (?1, ?2)",
            params![tag.id, tag.name],
        )?;
    }
    Ok(())
}

/// 标签设置/增删/改名/删除的逆操作；所有关系变更都可重复执行。
pub(crate) fn undo_tags(
    database: &LibraryDb,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let records = load_tag_records(&transaction, operation_id)?;
        let mut redo_names: Vec<TagRefRecord> = Vec::new();
        let mut redo_sets: Vec<TagSetState> = Vec::new();
        let mut redo_adds: Vec<(i64, i64)> = Vec::new();
        let mut redo_removals: Vec<(i64, i64)> = Vec::new();
        let mut redo_deletions: Vec<i64> = Vec::new();

        match kind {
            HistoryKind::TagSet => {
                let mut created = Vec::new();
                for record in &records {
                    let TagRecord::Set {
                        image_id,
                        old_tag_ids,
                        created: record_created,
                    } = record
                    else {
                        continue;
                    };
                    created.extend(record_created.iter().cloned());
                    // 两侧都按 `tag_id` 升序，可以直接判等；相同说明这条记录已经撤过。
                    let current: Vec<i64> = {
                        let mut statement = transaction.prepare(
                            "SELECT tag_id FROM image_tags WHERE image_id = ?1 ORDER BY tag_id",
                        )?;
                        statement
                            .query_map([image_id], |row| row.get(0))?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    if current == *old_tag_ids {
                        continue;
                    }
                    let tags: Vec<TagRefRecord> = record_created
                        .iter()
                        .filter(|tag| current.contains(&tag.id))
                        .cloned()
                        .collect();
                    redo_sets.push(TagSetState {
                        image_id: *image_id,
                        tag_ids: current,
                        tags,
                    });
                    transaction
                        .execute("DELETE FROM image_tags WHERE image_id = ?1", [image_id])?;
                    for tag_id in old_tag_ids {
                        insert_tag_pair(&transaction, *image_id, *tag_id)?;
                    }
                }
                redo_names.extend(delete_unreferenced_tags(&transaction, &created)?);
            }
            HistoryKind::TagAdd => {
                let mut created = Vec::new();
                for record in &records {
                    let TagRecord::Add {
                        pairs,
                        created: record_created,
                    } = record
                    else {
                        continue;
                    };
                    created.extend(record_created.iter().cloned());
                    let mut present = Vec::new();
                    for (image_id, tag_id) in pairs {
                        if transaction.execute(
                            "DELETE FROM image_tags WHERE image_id = ?1 AND tag_id = ?2",
                            params![image_id, tag_id],
                        )? > 0
                        {
                            present.push((*image_id, *tag_id));
                        }
                    }
                    redo_adds.extend(present);
                }
                redo_names.extend(delete_unreferenced_tags(&transaction, &created)?);
            }
            HistoryKind::TagRemove => {
                for record in &records {
                    let TagRecord::Remove { pairs } = record else {
                        continue;
                    };
                    for (image_id, tag_id) in pairs {
                        if insert_tag_pair(&transaction, *image_id, *tag_id)? {
                            redo_removals.push((*image_id, *tag_id));
                        }
                    }
                }
            }
            HistoryKind::TagRename => {
                for record in &records {
                    let TagRecord::Rename { tag_id, old_name } = record else {
                        continue;
                    };
                    let current: Option<String> = transaction
                        .query_row("SELECT name FROM tags WHERE id = ?1", [tag_id], |row| {
                            row.get(0)
                        })
                        .optional()?;
                    let Some(current) = current else {
                        continue;
                    };
                    if current == *old_name {
                        continue;
                    }
                    let duplicate: Option<i64> = transaction
                        .query_row(
                            "SELECT id FROM tags
                             WHERE name = ?1 COLLATE NOCASE AND id <> ?2",
                            params![old_name, tag_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if duplicate.is_some() {
                        return Err(AppError::message("已经有同名标签，无法恢复"));
                    }
                    transaction.execute(
                        "UPDATE tags SET name = ?1 WHERE id = ?2",
                        params![old_name, tag_id],
                    )?;
                    redo_names.push(TagRefRecord {
                        id: *tag_id,
                        name: current,
                    });
                }
            }
            HistoryKind::TagDelete => {
                // 一条记录可能是删一个标签，也可能是一次清理多个 0 张标签：
                // 每个 `Delete` 表头后面跟着它自己的关系清单，顺序即归属。
                // 分组内容是 `(标签 id, 名称, 关系)`。
                type TagGroup = (i64, String, Vec<(i64, i64)>);
                let mut groups: Vec<TagGroup> = Vec::new();
                for record in &records {
                    match record {
                        TagRecord::Delete { tag_id, name } => {
                            groups.push((*tag_id, name.clone(), Vec::new()))
                        }
                        TagRecord::Pairs { pairs: batch } => {
                            if let Some(last) = groups.last_mut() {
                                last.2.extend(batch.iter().copied());
                            }
                        }
                        _ => {}
                    }
                }
                if groups.is_empty() {
                    return Err(AppError::message("这条记录缺少撤销数据"));
                }
                for (tag_id, name, pairs) in groups {
                    let existing: Option<i64> = transaction
                        .query_row(
                            "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
                            [&name],
                            |row| row.get(0),
                        )
                        .optional()?;
                    let target_id = if let Some(existing_id) = existing {
                        // 同名标签已存在（例如崩溃续做）：合并关系，重做时只移除关系。
                        existing_id
                    } else {
                        transaction.execute(
                            "INSERT INTO tags(id, name) VALUES (?1, ?2)",
                            params![tag_id, name],
                        )?;
                        redo_deletions.push(tag_id);
                        tag_id
                    };
                    let mut restored = Vec::new();
                    for (image_id, _) in &pairs {
                        if insert_tag_pair(&transaction, *image_id, target_id)? {
                            restored.push((*image_id, target_id));
                        }
                    }
                    // 重建的标签重做时整行删除（关系随外键一起消失）；同名合并时只回退关系。
                    if target_id != tag_id {
                        redo_removals.extend(restored);
                    }
                }
            }
            HistoryKind::TagMerge => {
                let mut header: Option<(i64, String, i64)> = None;
                let mut images: Vec<i64> = Vec::new();
                let mut added: Vec<i64> = Vec::new();
                for record in &records {
                    match record {
                        TagRecord::Merge {
                            source_id,
                            source_name,
                            target_id,
                        } => header = Some((*source_id, source_name.clone(), *target_id)),
                        TagRecord::Pairs { pairs } => {
                            images.extend(pairs.iter().map(|(image_id, _)| *image_id));
                        }
                        TagRecord::MergeAdded { image_ids } => {
                            added.extend(image_ids.iter().copied())
                        }
                        _ => {}
                    }
                }
                let Some((source_id, source_name, target_id)) = header else {
                    return Err(AppError::message("这条记录缺少撤销数据"));
                };
                // 已经撤过（源标签又在了）就不再动，保证可重复执行。
                let present: bool = transaction.query_row(
                    "SELECT EXISTS (SELECT 1 FROM tags WHERE id = ?1)",
                    [source_id],
                    |row| row.get(0),
                )?;
                if !present {
                    // 1. 把这次加到目标上的关系撤回：目标要回到合并前的样子。
                    for image_id in &added {
                        if transaction.execute(
                            "DELETE FROM image_tags WHERE image_id = ?1 AND tag_id = ?2",
                            params![image_id, target_id],
                        )? > 0
                        {
                            redo_adds.push((*image_id, target_id));
                        }
                    }
                    // 2. 重建源标签。同名标签已经存在时把关系插回它（与 `tagDelete` 同一处理）。
                    let existing: Option<i64> = transaction
                        .query_row(
                            "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
                            [&source_name],
                            |row| row.get(0),
                        )
                        .optional()?;
                    let restore_id = match existing {
                        Some(id) => id,
                        None => {
                            transaction.execute(
                                "INSERT INTO tags(id, name) VALUES (?1, ?2)",
                                params![source_id, source_name],
                            )?;
                            redo_deletions.push(source_id);
                            source_id
                        }
                    };
                    let mut restored = Vec::new();
                    for image_id in &images {
                        if insert_tag_pair(&transaction, *image_id, restore_id)? {
                            restored.push((*image_id, restore_id));
                        }
                    }
                    if restore_id != source_id {
                        redo_removals.extend(restored);
                    }
                }
            }
            _ => {}
        }

        if !redo_names.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::TagNames { items: redo_names },
            )?;
        }
        if !redo_sets.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::TagSets { items: redo_sets },
            )?;
        }
        if !redo_adds.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::TagAdds { pairs: redo_adds },
            )?;
        }
        if !redo_removals.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::TagRemovals {
                    pairs: redo_removals,
                },
            )?;
        }
        if !redo_deletions.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::TagDeletions {
                    ids: redo_deletions,
                },
            )?;
        }
        transaction.commit()?;
        Ok(())
    })
}

pub(crate) fn redo_tags(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut applied = false;
        // `undo_tags` 按“改名 → 关系状态 → 加关系 → 减关系 → 删标签”的顺序追加 redo，
        // 按 id 依次执行即可：标签行先补回，指向它的关系才插得进去。
        for record in load_redo_records(&transaction, operation_id)? {
            match record {
                RedoRecord::TagNames { items } => {
                    for tag in &items {
                        ensure_tag_row(&transaction, tag)?;
                    }
                    applied |= !items.is_empty();
                }
                RedoRecord::TagSets { items } => {
                    for item in &items {
                        for tag in &item.tags {
                            ensure_tag_row(&transaction, tag)?;
                        }
                        transaction.execute(
                            "DELETE FROM image_tags WHERE image_id = ?1",
                            [item.image_id],
                        )?;
                        for tag_id in &item.tag_ids {
                            insert_tag_pair(&transaction, item.image_id, *tag_id)?;
                        }
                    }
                    applied |= !items.is_empty();
                }
                RedoRecord::TagAdds { pairs } => {
                    for (image_id, tag_id) in &pairs {
                        insert_tag_pair(&transaction, *image_id, *tag_id)?;
                    }
                    applied |= !pairs.is_empty();
                }
                RedoRecord::TagRemovals { pairs } => {
                    for (image_id, tag_id) in &pairs {
                        transaction.execute(
                            "DELETE FROM image_tags WHERE image_id = ?1 AND tag_id = ?2",
                            params![image_id, tag_id],
                        )?;
                    }
                    applied |= !pairs.is_empty();
                }
                RedoRecord::TagDeletions { ids } => {
                    for id in &ids {
                        transaction.execute("DELETE FROM tags WHERE id = ?1", [id])?;
                    }
                    applied |= !ids.is_empty();
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

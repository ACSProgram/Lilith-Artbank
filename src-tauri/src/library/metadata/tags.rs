use std::collections::HashSet;

use rusqlite::{
    Connection, OptionalExtension, Transaction, params, params_from_iter, types::Value,
};
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        history::{self, HistoryKind, NewOperation, TagRecord, TagRefRecord},
        import::ensure_tags_tracked,
        placeholders, temp,
    },
};

use super::{
    MAX_BATCH_IDS, SQL_CHUNK,
    fields::{normalize_tag_names, tag_ids_for_names, tag_refs, validate_tag_name},
    read::TagRef,
    validate_ids,
};

pub fn set_image_tags(
    database: &LibraryDb,
    image_id: i64,
    tag_names: &[String],
) -> AppResult<Vec<TagRef>> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    let names = normalize_tag_names(tag_names)?;
    let label = if names.is_empty() {
        "清空标签".to_owned()
    } else {
        format!("设置标签「{}」", tag_summary(&names))
    };
    let tracked = !temp::is_image(database, image_id)?;
    history::write_with_id_mode(
        database,
        NewOperation::new(HistoryKind::TagSet, label),
        tracked,
        |transaction, draft| {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS (SELECT 1 FROM images WHERE id = ?1)",
                [image_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(AppError::message("图片不存在"));
            }
            let old_tag_ids: Vec<i64> = {
                let mut statement = transaction
                    .prepare("SELECT tag_id FROM image_tags WHERE image_id = ?1 ORDER BY tag_id")?;
                statement
                    .query_map([image_id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut created = Vec::new();
            let tag_ids = ensure_tags_tracked(transaction, &names, &mut created)?;
            let mut sorted = tag_ids.clone();
            sorted.sort_unstable();
            if old_tag_ids == sorted && created.is_empty() {
                return Ok(());
            }
            transaction.execute("DELETE FROM image_tags WHERE image_id = ?1", [image_id])?;
            for tag_id in &tag_ids {
                transaction.execute(
                    "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)",
                    params![image_id, tag_id],
                )?;
            }
            if tracked {
                draft.json(&TagRecord::Set {
                    image_id,
                    old_tag_ids,
                    created: created
                        .into_iter()
                        .map(|(id, name)| TagRefRecord { id, name })
                        .collect(),
                })?;
                draft.affected(1);
            }
            Ok(())
        },
    )?;
    tag_refs(database, image_id)
}

pub fn add_tags(database: &LibraryDb, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
    add_tags_with_id(database, image_ids, tag_names).map(|(changed, _)| changed)
}

pub fn add_tags_with_id(
    database: &LibraryDb,
    image_ids: &[i64],
    tag_names: &[String],
) -> AppResult<(u64, Option<i64>)> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    let names = normalize_tag_names(tag_names)?;
    if names.is_empty() {
        return Ok((0, None));
    }
    let temp_ids = temp::ids_in(database, &ids)?;
    let tracked = ids.iter().any(|id| !temp_ids.contains(id));
    history::write_with_id_mode(
        database,
        NewOperation::new(
            HistoryKind::TagAdd,
            format!("添加标签「{}」", tag_summary(&names)),
        ),
        tracked,
        |transaction, draft| {
            let mut created = Vec::new();
            let tag_ids = ensure_tags_tracked(transaction, &names, &mut created)?;
            let mut pending_created: Vec<TagRefRecord> = created
                .into_iter()
                .map(|(id, name)| TagRefRecord { id, name })
                .collect();
            let mut added = 0_u64;
            let mut recorded = 0_u64;
            for tag_id in &tag_ids {
                for chunk in ids.chunks(SQL_CHUNK) {
                    let marks = placeholders(chunk.len());
                    let existing_images: HashSet<i64> = {
                        let mut statement = transaction
                            .prepare(&format!("SELECT id FROM images WHERE id IN ({marks})"))?;
                        statement
                            .query_map(params_from_iter(chunk.iter()), |row| row.get(0))?
                            .collect::<rusqlite::Result<HashSet<_>>>()?
                    };
                    let existing_relations: HashSet<i64> = {
                        let mut statement = transaction.prepare(&format!(
                            "SELECT image_id FROM image_tags
                             WHERE tag_id = ?1 AND image_id IN ({marks})"
                        ))?;
                        statement
                            .query_map(
                                params_from_iter(
                                    std::iter::once(Value::Integer(*tag_id))
                                        .chain(chunk.iter().map(|id| Value::Integer(*id))),
                                ),
                                |row| row.get(0),
                            )?
                            .collect::<rusqlite::Result<HashSet<_>>>()?
                    };
                    let mut pairs = Vec::new();
                    for image_id in chunk {
                        if !existing_images.contains(image_id)
                            || existing_relations.contains(image_id)
                        {
                            continue;
                        }
                        let changed = transaction.execute(
                            "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)
                             ON CONFLICT(image_id, tag_id) DO NOTHING",
                            params![image_id, tag_id],
                        )?;
                        if changed > 0 {
                            pairs.push((*image_id, *tag_id));
                        }
                    }
                    if pairs.is_empty() {
                        continue;
                    }
                    added += pairs.len() as u64;
                    if tracked {
                        pairs.retain(|(image_id, _)| !temp_ids.contains(image_id));
                        recorded += pairs.len() as u64;
                        if !pairs.is_empty() || !pending_created.is_empty() {
                            draft.json(&TagRecord::Add {
                                pairs,
                                created: std::mem::take(&mut pending_created),
                            })?;
                        }
                    }
                }
            }
            if tracked && !pending_created.is_empty() {
                draft.json(&TagRecord::Add {
                    pairs: Vec::new(),
                    created: pending_created,
                })?;
            }
            if tracked {
                draft.affected(recorded);
            }
            Ok(added)
        },
    )
}

pub fn remove_tags(
    database: &LibraryDb,
    image_ids: &[i64],
    tag_names: &[String],
) -> AppResult<u64> {
    remove_tags_with_id(database, image_ids, tag_names).map(|(changed, _)| changed)
}

pub fn remove_tags_with_id(
    database: &LibraryDb,
    image_ids: &[i64],
    tag_names: &[String],
) -> AppResult<(u64, Option<i64>)> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    let names = normalize_tag_names(tag_names)?;
    if names.is_empty() {
        return Ok((0, None));
    }
    let temp_ids = temp::ids_in(database, &ids)?;
    let tracked = ids.iter().any(|id| !temp_ids.contains(id));
    history::write_with_id_mode(
        database,
        NewOperation::new(
            HistoryKind::TagRemove,
            format!("移除标签「{}」", tag_summary(&names)),
        ),
        tracked,
        |transaction, draft| {
            let tag_ids = tag_ids_for_names(transaction, &names)?;
            let mut removed = 0_u64;
            let mut recorded = 0_u64;
            for tag_id in tag_ids {
                for chunk in ids.chunks(SQL_CHUNK) {
                    let marks = placeholders(chunk.len());
                    let values = || {
                        params_from_iter(
                            std::iter::once(Value::Integer(tag_id))
                                .chain(chunk.iter().map(|id| Value::Integer(*id))),
                        )
                    };
                    // 先查出现有关系：既得到撤销用的 `(imageId, tagId)`，也避免把没改动的行计入影响数。
                    let present: Vec<(i64, i64)> = {
                        let mut statement = transaction.prepare(&format!(
                            "SELECT image_id FROM image_tags
                             WHERE tag_id = ?1 AND image_id IN ({marks})"
                        ))?;
                        statement
                            .query_map(values(), |row| Ok((row.get::<_, i64>(0)?, tag_id)))?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    if present.is_empty() {
                        continue;
                    }
                    transaction.execute(
                        &format!(
                            "DELETE FROM image_tags
                             WHERE tag_id = ?1 AND image_id IN ({marks})"
                        ),
                        values(),
                    )?;
                    removed += present.len() as u64;
                    if tracked {
                        let pairs: Vec<(i64, i64)> = present
                            .into_iter()
                            .filter(|(image_id, _)| !temp_ids.contains(image_id))
                            .collect();
                        recorded += pairs.len() as u64;
                        if !pairs.is_empty() {
                            draft.json(&TagRecord::Remove { pairs })?;
                        }
                    }
                }
            }
            if tracked {
                draft.affected(recorded);
            }
            Ok(removed)
        },
    )
}

pub fn rename_tag(database: &LibraryDb, tag_id: i64, name: &str) -> AppResult<()> {
    rename_tag_with_id(database, tag_id, name).map(|_| ())
}

pub fn rename_tag_with_id(database: &LibraryDb, tag_id: i64, name: &str) -> AppResult<Option<i64>> {
    if tag_id <= 0 {
        return Err(AppError::message("标签 id 无效"));
    }
    let name = validate_tag_name(name)?;
    let (_, operation_id) = history::write_with_id(
        database,
        NewOperation::new(HistoryKind::TagRename, format!("重命名标签「{name}」")),
        |transaction, draft| {
            let old_name: Option<String> = transaction
                .query_row("SELECT name FROM tags WHERE id = ?1", [tag_id], |row| {
                    row.get(0)
                })
                .optional()?;
            let Some(old_name) = old_name else {
                return Err(AppError::message("标签不存在"));
            };
            if old_name == name {
                return Ok(());
            }
            let duplicate: Option<i64> = transaction
                .query_row(
                    "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE AND id <> ?2",
                    params![name, tag_id],
                    |row| row.get(0),
                )
                .optional()?;
            if duplicate.is_some() {
                return Err(AppError::message("已经有同名标签"));
            }
            transaction.execute(
                "UPDATE tags SET name = ?1 WHERE id = ?2",
                params![name, tag_id],
            )?;
            draft.json(&TagRecord::Rename { tag_id, old_name })?;
            draft.affected(1);
            Ok(())
        },
    )?;
    Ok(operation_id)
}

pub fn delete_tag(database: &LibraryDb, tag_id: i64) -> AppResult<u64> {
    delete_tag_with_id(database, tag_id).map(|(affected, _)| affected)
}

pub fn delete_tag_with_id(database: &LibraryDb, tag_id: i64) -> AppResult<(u64, Option<i64>)> {
    if tag_id <= 0 {
        return Err(AppError::message("标签 id 无效"));
    }
    let label = database
        .with_reader(|connection| {
            Ok(connection
                .query_row("SELECT name FROM tags WHERE id = ?1", [tag_id], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?)
        })?
        .map(|name| format!("删除标签「{name}」"))
        .unwrap_or_else(|| "删除标签".to_owned());
    history::write_with_id(
        database,
        NewOperation::new(HistoryKind::TagDelete, label),
        |transaction, draft| {
            let name: Option<String> = transaction
                .query_row("SELECT name FROM tags WHERE id = ?1", [tag_id], |row| {
                    row.get(0)
                })
                .optional()?;
            let Some(name) = name else {
                return Err(AppError::message("标签不存在"));
            };
            let image_ids: Vec<i64> = {
                let mut statement = transaction.prepare(
                    "SELECT image_id FROM image_tags WHERE tag_id = ?1 ORDER BY image_id",
                )?;
                statement
                    .query_map([tag_id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            draft.json(&TagRecord::Delete {
                tag_id,
                name: name.clone(),
            })?;
            for chunk in image_ids.chunks(1_000) {
                draft.json(&TagRecord::Pairs {
                    pairs: chunk.iter().map(|id| (*id, tag_id)).collect(),
                })?;
            }
            transaction.execute("DELETE FROM tags WHERE id = ?1", [tag_id])?;
            draft.affected(image_ids.len() as u64);
            Ok(image_ids.len() as u64)
        },
    )
}

/// 一次删掉所有「0 张」标签：判定与 `list_tags` 同口径（可见图片数为 0）。
///
/// 整次操作写**一条** `tagDelete` 记录：382 个标签各写一条会把记录面板冲垮。
/// 撤销时按记录里的 `(id, name)` 把标签行插回，关系（只可能指向回收站或临时区）
/// 一并恢复，不涉及任何正常图片。
pub fn delete_zero_tags(database: &LibraryDb) -> AppResult<u64> {
    history::write(
        database,
        NewOperation::new(HistoryKind::TagDelete, "清理 0 张标签"),
        |transaction, draft| {
            let zero: Vec<(i64, String)> = {
                let mut statement = transaction.prepare(&format!(
                    "SELECT tags.id, tags.name FROM tags
                 WHERE NOT EXISTS (
                     SELECT 1 FROM image_tags
                     JOIN images ON images.id = image_tags.image_id
                     WHERE image_tags.tag_id = tags.id
                       AND images.deleted_at IS NULL
                       AND {exclude}
                 )
                 ORDER BY tags.id",
                    exclude = temp::EXCLUDE_SQL
                ))?;
                statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if zero.is_empty() {
                return Ok(0);
            }
            for (tag_id, name) in &zero {
                draft.json(&TagRecord::Delete {
                    tag_id: *tag_id,
                    name: name.clone(),
                })?;
                let pairs: Vec<(i64, i64)> = {
                    let mut statement =
                        transaction.prepare("SELECT image_id FROM image_tags WHERE tag_id = ?1")?;
                    statement
                        .query_map([tag_id], |row| Ok((row.get::<_, i64>(0)?, *tag_id)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                for chunk in pairs.chunks(1_000) {
                    draft.json(&TagRecord::Pairs {
                        pairs: chunk.to_vec(),
                    })?;
                }
                transaction.execute("DELETE FROM tags WHERE id = ?1", [tag_id])?;
            }
            draft.affected(zero.len() as u64);
            Ok(zero.len() as u64)
        },
    )
}

/// 合并两个标签：把源标签的全部图片关系迁到目标标签，然后删掉源标签行。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeReport {
    /// 源标签原本挂了多少张图片。
    pub source_images: u64,
    /// 其中目标标签原本没有、这次新加上的关系数。
    pub moved: u64,
    /// 其中目标标签本来就有、只做了去重的关系数。
    pub duplicate: u64,
    pub source_name: String,
    pub target_name: String,
}

pub fn merge_tags_with_id(
    database: &LibraryDb,
    source_id: i64,
    target_id: i64,
) -> AppResult<(MergeReport, Option<i64>)> {
    if source_id <= 0 || target_id <= 0 {
        return Err(AppError::message("标签 id 无效"));
    }
    if source_id == target_id {
        return Err(AppError::message("不能把标签合并到自己"));
    }
    // 标题在事务里才知道两个名称，先写占位再就地改写。
    let (report, operation_id) = history::write_with_id(
        database,
        NewOperation::new(HistoryKind::TagMerge, "合并标签".to_owned()),
        |transaction, draft| merge_in_transaction(transaction, draft, source_id, target_id),
    )?;
    if let Some(operation_id) = operation_id {
        let label = format!(
            "合并标签「{}」→「{}」",
            report.source_name, report.target_name
        );
        history::update_label(database, operation_id, &label)?;
    }
    Ok((report, operation_id))
}

/// 供 MCP 预先统计“会变动多少”，不写库。
pub fn merge_preview(
    database: &LibraryDb,
    source_id: i64,
    target_id: i64,
) -> AppResult<MergeReport> {
    if source_id <= 0 || target_id <= 0 {
        return Err(AppError::message("标签 id 无效"));
    }
    if source_id == target_id {
        return Err(AppError::message("不能把标签合并到自己"));
    }
    database.with_reader(|connection| {
        let source_name =
            tag_name_in(connection, source_id)?.ok_or_else(|| AppError::message("源标签不存在"))?;
        let target_name = tag_name_in(connection, target_id)?
            .ok_or_else(|| AppError::message("目标标签不存在"))?;
        let source_images: i64 = connection.query_row(
            "SELECT count(*) FROM image_tags WHERE tag_id = ?1",
            [source_id],
            |row| row.get(0),
        )?;
        let duplicate: i64 = connection.query_row(
            "SELECT count(*) FROM image_tags source
             WHERE source.tag_id = ?1
               AND EXISTS (SELECT 1 FROM image_tags target
                           WHERE target.tag_id = ?2 AND target.image_id = source.image_id)",
            params![source_id, target_id],
            |row| row.get(0),
        )?;
        Ok(MergeReport {
            source_images: source_images.max(0) as u64,
            moved: (source_images - duplicate).max(0) as u64,
            duplicate: duplicate.max(0) as u64,
            source_name,
            target_name,
        })
    })
}

pub fn tag_name(database: &LibraryDb, tag_id: i64) -> AppResult<Option<String>> {
    database.with_reader(|connection| tag_name_in(connection, tag_id))
}

fn tag_name_in(connection: &Connection, tag_id: i64) -> AppResult<Option<String>> {
    Ok(connection
        .query_row("SELECT name FROM tags WHERE id = ?1", [tag_id], |row| {
            row.get::<_, String>(0)
        })
        .optional()?)
}

pub fn tag_image_count(database: &LibraryDb, tag_id: i64) -> AppResult<u64> {
    database.with_reader(|connection| {
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM image_tags WHERE tag_id = ?1",
            [tag_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    })
}

fn merge_in_transaction(
    transaction: &Transaction<'_>,
    draft: &mut history::OperationDraft<'_>,
    source_id: i64,
    target_id: i64,
) -> AppResult<MergeReport> {
    let source_name =
        tag_name_in(transaction, source_id)?.ok_or_else(|| AppError::message("源标签不存在"))?;
    let target_name =
        tag_name_in(transaction, target_id)?.ok_or_else(|| AppError::message("目标标签不存在"))?;
    let images: Vec<i64> = {
        let mut statement = transaction
            .prepare("SELECT image_id FROM image_tags WHERE tag_id = ?1 ORDER BY image_id")?;
        statement
            .query_map([source_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut added = Vec::new();
    for image_id in &images {
        let changed = transaction.execute(
            "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)
             ON CONFLICT(image_id, tag_id) DO NOTHING",
            params![image_id, target_id],
        )?;
        if changed > 0 {
            added.push(*image_id);
        }
    }
    transaction.execute("DELETE FROM tags WHERE id = ?1", [source_id])?;
    let source_images = images.len() as u64;
    let report = MergeReport {
        source_images,
        moved: added.len() as u64,
        duplicate: source_images - added.len() as u64,
        source_name: source_name.clone(),
        target_name,
    };
    draft.json(&TagRecord::Merge {
        source_id,
        source_name,
        target_id,
    })?;
    // 关系清单按 1000 条一个 chunk，与标签删除的记录分块口径一致。
    for chunk in images.chunks(1_000) {
        draft.json(&TagRecord::Pairs {
            pairs: chunk
                .iter()
                .map(|image_id| (*image_id, source_id))
                .collect(),
        })?;
    }
    for chunk in added.chunks(1_000) {
        draft.json(&TagRecord::MergeAdded {
            image_ids: chunk.to_vec(),
        })?;
    }
    draft.affected(source_images);
    Ok(report)
}

fn tag_summary(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(3).map(String::as_str).collect();
    let mut text = shown.join("、");
    if names.len() > shown.len() {
        text.push('…');
    }
    text
}

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        history::{self, ChunkFormat, DescriptionRecord, HistoryKind, NewOperation},
        placeholders,
        service::now_ms,
        temp,
    },
};

use super::{MAX_BATCH_IDS, SQL_CHUNK, read::TagRef, validate_ids};

const MAX_TAGS_PER_IMAGE: usize = 64;
const MAX_TAG_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 50_000;

pub fn update_score(database: &LibraryDb, image_id: i64, score: i64) -> AppResult<()> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    validate_score(score)?;
    let tracked = !temp::is_image(database, image_id)?;
    history::write_with_id_mode(
        database,
        NewOperation::new(HistoryKind::Score, format!("评分 {score} 星")),
        tracked,
        |transaction, draft| {
            let old: Option<(i64, i64)> = transaction
                .query_row(
                    "SELECT score, modified_at FROM images WHERE id = ?1",
                    [image_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((old_score, old_modified_at)) = old else {
                return Err(AppError::message("图片不存在"));
            };
            transaction.execute(
                "UPDATE images SET score = ?1, modified_at = ?2 WHERE id = ?3",
                params![score, now_ms(), image_id],
            )?;
            if tracked {
                draft.triples(&[(image_id, old_score, old_modified_at)]);
                draft.affected(1);
            }
            Ok(())
        },
    )?;
    Ok(())
}

pub fn set_scores(database: &LibraryDb, image_ids: &[i64], score: i64) -> AppResult<u64> {
    set_scores_with_id(database, image_ids, score).map(|(changed, _)| changed)
}

pub fn set_scores_with_id(
    database: &LibraryDb,
    image_ids: &[i64],
    score: i64,
) -> AppResult<(u64, Option<i64>)> {
    validate_score(score)?;
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    let temp_ids = temp::ids_in(database, &ids)?;
    let tracked = ids.iter().any(|id| !temp_ids.contains(id));
    history::write_with_id_mode(
        database,
        NewOperation::new(HistoryKind::Score, format!("批量评分 {score} 星")),
        tracked,
        |transaction, draft| {
            let now = now_ms();
            let mut changed = 0_u64;
            let mut recorded = 0_u64;
            let mut triples = Vec::new();
            for chunk in ids.chunks(SQL_CHUNK) {
                let marks = placeholders(chunk.len());
                let old_rows = {
                    let mut statement = transaction.prepare(&format!(
                        "SELECT id, score, modified_at FROM images WHERE id IN ({marks})"
                    ))?;
                    statement
                        .query_map(params_from_iter(chunk.iter()), |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                for row in old_rows {
                    if temp_ids.contains(&row.0) {
                        continue;
                    }
                    triples.push(row);
                    recorded += 1;
                }
                changed += transaction.execute(
                    &format!(
                        "UPDATE images SET score = ?1, modified_at = ?2 WHERE id IN ({marks})"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(score))
                            .chain(std::iter::once(Value::Integer(now)))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )? as u64;
            }
            if tracked {
                draft.triples(&triples);
                draft.affected(recorded);
            }
            Ok(changed)
        },
    )
}

pub fn update_description(database: &LibraryDb, image_id: i64, description: &str) -> AppResult<()> {
    update_description_with_id(database, image_id, description).map(|(_, _)| ())
}

/// 与其他 `*_with_id` 一致：返回实际改动的图片数（0 或 1）与新记录 id。
pub fn update_description_with_id(
    database: &LibraryDb,
    image_id: i64,
    description: &str,
) -> AppResult<(u64, Option<i64>)> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    let description = description.trim();
    validate_description(description)?;
    let tracked = !temp::is_image(database, image_id)?;
    history::write_with_id_mode(
        database,
        NewOperation::new(HistoryKind::Description, "修改描述"),
        tracked,
        |transaction, draft| {
            let old: Option<(String, String, String, i64)> = transaction
                .query_row(
                    "SELECT description, description_source, description_model, modified_at
                     FROM images WHERE id = ?1",
                    [image_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((old_description, old_source, old_model, old_modified_at)) = old else {
                return Err(AppError::message("图片不存在"));
            };
            if old_description == description && old_source == "user" && old_model.is_empty() {
                return Ok(0);
            }
            let modified_at = now_ms();
            transaction.execute(
                "UPDATE images
                 SET description = ?1, description_source = 'user', description_model = '',
                     modified_at = ?2
                 WHERE id = ?3",
                params![description, modified_at, image_id],
            )?;
            if tracked {
                draft.json(&DescriptionRecord {
                    image_id,
                    description: old_description,
                    description_source: old_source,
                    description_model: old_model,
                    modified_at: old_modified_at,
                })?;
                draft.affected(1);
            }
            Ok(1)
        },
    )
}

/// AI 描述只覆盖空描述或非用户来源；手写描述默认受保护。
pub fn set_ai_description(
    database: &LibraryDb,
    image_id: i64,
    description: &str,
    model: &str,
) -> AppResult<bool> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    let description = description.trim();
    if description.is_empty() {
        return Err(AppError::message("AI 描述为空"));
    }
    validate_description(description)?;
    let tracked = !temp::is_image(database, image_id)?;
    let (changed, _) = history::write_with_id_mode(
        database,
        NewOperation::new(HistoryKind::AiDescription, "AI 生成描述"),
        tracked,
        |transaction, draft| {
            let Some(old) = write_ai_description(transaction, image_id, description, model)? else {
                return Ok(false);
            };
            if tracked {
                draft.json(&old)?;
                draft.affected(1);
            }
            Ok(true)
        },
    )?;
    Ok(changed)
}

/// 批量 AI 描述：在任务开始前用 `begin_live` 建记录，每张图的事务里追加旧描述与影响数。
pub fn set_ai_description_live(
    database: &LibraryDb,
    operation_id: i64,
    image_id: i64,
    description: &str,
    model: &str,
) -> AppResult<bool> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    let description = description.trim();
    if description.is_empty() {
        return Err(AppError::message("AI 描述为空"));
    }
    validate_description(description)?;
    database.with_writer(|connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(old) = write_ai_description(&transaction, image_id, description, model)? else {
            return Ok(false);
        };
        if !temp::is_image_in(&transaction, image_id)? {
            history::discard_redo_in_tx(&transaction)?;
            history::append_live_in_tx(
                &transaction,
                operation_id,
                ChunkFormat::Json,
                &serde_json::to_vec(&old)?,
            )?;
            history::add_live_affected_in_tx(&transaction, operation_id, 1)?;
        }
        transaction.commit()?;
        Ok(true)
    })
}

fn validate_description(description: &str) -> AppResult<()> {
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(AppError::message(format!(
            "描述不能超过 {MAX_DESCRIPTION_CHARS} 个字符"
        )));
    }
    Ok(())
}

/// 写入 AI 描述并在返回值里带上撤销所需的旧值；手写描述受保护、内容未变时返回 `None`。
fn write_ai_description(
    connection: &rusqlite::Connection,
    image_id: i64,
    description: &str,
    model: &str,
) -> AppResult<Option<DescriptionRecord>> {
    let old: Option<(String, String, String, i64)> = connection
        .query_row(
            "SELECT description, description_source, description_model, modified_at
             FROM images WHERE id = ?1",
            [image_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((old_description, old_source, old_model, old_modified_at)) = old else {
        return Err(AppError::message("图片不存在"));
    };
    if !old_description.is_empty() && old_source == "user" {
        return Ok(None);
    }
    if old_description == description && old_source == "ai" && old_model == model {
        return Ok(None);
    }
    connection.execute(
        "UPDATE images
         SET description = ?1, description_source = 'ai', description_model = ?2,
             modified_at = ?3
         WHERE id = ?4",
        params![description, model, now_ms(), image_id],
    )?;
    Ok(Some(DescriptionRecord {
        image_id,
        description: old_description,
        description_source: old_source,
        description_model: old_model,
        modified_at: old_modified_at,
    }))
}

pub(super) fn tag_refs(database: &LibraryDb, image_id: i64) -> AppResult<Vec<TagRef>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT tags.id, tags.name
             FROM image_tags JOIN tags ON tags.id = image_tags.tag_id
             WHERE image_tags.image_id = ?1
             ORDER BY tags.name COLLATE NOCASE",
        )?;
        let tags = statement
            .query_map([image_id], |row| {
                Ok(TagRef {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(tags)
    })
}

pub(super) fn tag_ids_for_names(connection: &Connection, names: &[String]) -> AppResult<Vec<i64>> {
    let mut ids = Vec::new();
    for name in names {
        let id: Option<i64> = connection
            .query_row(
                "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            ids.push(id);
        }
    }
    Ok(ids)
}

pub(super) fn normalize_tag_names(names: &[String]) -> AppResult<Vec<String>> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        let name = validate_tag_name(name)?;
        if seen.insert(name.to_lowercase()) {
            result.push(name);
        }
        if result.len() > MAX_TAGS_PER_IMAGE {
            return Err(AppError::message(format!(
                "每张图片最多 {MAX_TAGS_PER_IMAGE} 个标签"
            )));
        }
    }
    Ok(result)
}

pub(super) fn validate_tag_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::message("标签名不能为空"));
    }
    if name.chars().count() > MAX_TAG_NAME_CHARS {
        return Err(AppError::message(format!(
            "标签名不能超过 {MAX_TAG_NAME_CHARS} 个字符"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(AppError::message("标签名包含控制字符"));
    }
    Ok(name.to_owned())
}

fn validate_score(score: i64) -> AppResult<()> {
    if !(0..=5).contains(&score) {
        return Err(AppError::message("评分必须在 0-5 之间"));
    }
    Ok(())
}

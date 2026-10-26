use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    library::{
        BatchReporter,
        db::LibraryDb,
        history::{
            self, ChunkFormat, DescriptionRecord, HistoryKind, NewOperation, TagRecord,
            TagRefRecord,
        },
        import::{ensure_folder_tracked, ensure_tags_tracked},
        paths::{LibraryPaths, normalize_content_extension, normalize_folder},
        placeholders,
        service::now_ms,
        temp,
    },
};

const SQL_CHUNK: usize = 400;
const MAX_BATCH_IDS: usize = 1_000_000;
const MAX_FILE_BATCH_IDS: usize = 10_000;
const MAX_TAGS_PER_IMAGE: usize = 64;
const MAX_TAG_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 50_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorEntry {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub weight: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDetails {
    pub id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub file_path: String,
    pub folder_id: i64,
    pub folder_path: String,
    pub file_size: i64,
    pub width: i64,
    pub height: i64,
    pub format: String,
    pub has_alpha: bool,
    pub score: i64,
    pub description: String,
    pub description_source: String,
    pub description_model: String,
    pub imported_at: i64,
    pub modified_at: i64,
    pub deleted_at: Option<i64>,
    pub thumb_version: i64,
    pub thumb_width: i64,
    pub thumb_height: i64,
    pub tags: Vec<TagRef>,
    pub colors4: Vec<ColorEntry>,
    pub colors8: Vec<ColorEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchFailure {
    pub image_id: Option<i64>,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MoveReport {
    pub moved: u64,
    pub skipped: u64,
    pub failures: Vec<BatchFailure>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReport {
    pub deleted: u64,
    pub files_removed: u64,
    /// 永久删除/清空回收站只做标记时仍为 true；文件在记录过期或清空记录后释放。
    pub deferred: bool,
    pub failures: Vec<BatchFailure>,
}

pub fn image_details(database: &LibraryDb, root: &Path, image_id: i64) -> AppResult<ImageDetails> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    database.with_reader(|connection| {
        let mut details = connection
            .query_row(
                "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                        folders.path, images.file_size, images.width, images.height,
                        images.format, images.has_alpha, images.score, images.description,
                        images.description_source, images.description_model,
                        images.imported_at, images.modified_at, images.deleted_at,
                        images.thumb_version, images.thumb_width, images.thumb_height,
                        images.colors4, images.colors8
                 FROM images
                 JOIN folders ON folders.id = images.folder_id
                 WHERE images.id = ?1",
                [image_id],
                |row| {
                    Ok(ImageDetails {
                        id: row.get(0)?,
                        content_hash: row.get(1)?,
                        file_ext: row.get(2)?,
                        folder_id: row.get(3)?,
                        folder_path: row.get(4)?,
                        file_size: row.get(5)?,
                        width: row.get(6)?,
                        height: row.get(7)?,
                        format: row.get(8)?,
                        has_alpha: row.get::<_, i64>(9)? != 0,
                        score: row.get(10)?,
                        description: row.get(11)?,
                        description_source: row.get(12)?,
                        description_model: row.get(13)?,
                        imported_at: row.get(14)?,
                        modified_at: row.get(15)?,
                        deleted_at: row.get(16)?,
                        thumb_version: row.get(17)?,
                        thumb_width: row.get(18)?,
                        thumb_height: row.get(19)?,
                        tags: Vec::new(),
                        colors4: parse_palette(&row.get::<_, Vec<u8>>(20)?),
                        colors8: parse_palette(&row.get::<_, Vec<u8>>(21)?),
                        file_path: String::new(),
                    })
                },
            )
            .optional()?
            .ok_or_else(|| AppError::message("图片不存在"))?;

        let file_name = format!("{}.{}", details.content_hash, details.file_ext);
        details.file_path = LibraryPaths::new(root)
            .content_file(&details.folder_path, &file_name)
            .to_string_lossy()
            .into_owned();

        let mut statement = connection.prepare(
            "SELECT tags.id, tags.name
             FROM image_tags
             JOIN tags ON tags.id = image_tags.tag_id
             WHERE image_tags.image_id = ?1
             ORDER BY tags.name COLLATE NOCASE",
        )?;
        details.tags = statement
            .query_map([image_id], |row| {
                Ok(TagRef {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(details)
    })
}

fn parse_palette(bytes: &[u8]) -> Vec<ColorEntry> {
    bytes
        .chunks_exact(4)
        .map(|slot| ColorEntry {
            r: slot[0],
            g: slot[1],
            b: slot[2],
            weight: slot[3],
        })
        .collect()
}

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
    if tag_id <= 0 {
        return Err(AppError::message("标签 id 无效"));
    }
    let name = validate_tag_name(name)?;
    history::write(
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
    )
}

pub fn delete_tag(database: &LibraryDb, tag_id: i64) -> AppResult<u64> {
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
    history::write(
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

fn tag_summary(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(3).map(String::as_str).collect();
    let mut text = shown.join("、");
    if names.len() > shown.len() {
        text.push('…');
    }
    text
}

pub fn move_images(
    database: &LibraryDb,
    root: &Path,
    image_ids: &[i64],
    folder_path: &str,
    reporter: &dyn BatchReporter,
) -> AppResult<MoveReport> {
    move_images_with_id(database, root, image_ids, folder_path, reporter).map(|(report, _)| report)
}

pub fn move_images_with_id(
    database: &LibraryDb,
    root: &Path,
    image_ids: &[i64],
    folder_path: &str,
    reporter: &dyn BatchReporter,
) -> AppResult<(MoveReport, Option<i64>)> {
    let ids = validate_ids_for_files(image_ids)?;
    // 临时路径在 normalize_folder 里就会被拒（保留目录名），不需要再查一次。
    let folder_path = normalize_folder(folder_path)?;
    database.with_reader(|connection| {
        temp::reject_images_in(
            connection,
            &ids,
            "临时图片不能移出临时文件夹；关闭或切换图库时会自动清空",
        )
    })?;
    let paths = LibraryPaths::new(root);
    let total = ids.len();
    reporter.report(0, total);

    history::write_with_id(
        database,
        NewOperation::new(HistoryKind::Move, format!("移动图片到「{folder_path}」")),
        |transaction, draft| {
            let mut report = MoveReport::default();
            let mut created_folders = Vec::new();
            let target_folder =
                ensure_folder_tracked(transaction, &folder_path, now_ms(), &mut created_folders)?;
            let target_dir = if folder_path.is_empty() {
                paths.root.clone()
            } else {
                paths
                    .root
                    .join(folder_path.replace('/', std::path::MAIN_SEPARATOR_STR))
            };
            std::fs::create_dir_all(&target_dir)?;
            ensure_dir_inside_root(&paths.root, &target_dir)?;

            let now = now_ms();
            let mut done = 0_usize;
            let mut moves = Vec::new();
            for chunk in ids.chunks(SQL_CHUNK) {
                if reporter.cancelled() {
                    break;
                }
                let marks = placeholders(chunk.len());
                let mut statement = transaction.prepare(&format!(
                    "SELECT images.id, images.content_hash, images.file_ext, folders.path,
                            images.folder_id, images.modified_at
                     FROM images JOIN folders ON folders.id = images.folder_id
                     WHERE images.id IN ({marks})"
                ))?;
                let rows = statement
                    .query_map(params_from_iter(chunk.iter()), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);

                for (
                    image_id,
                    content_hash,
                    file_ext,
                    old_folder,
                    old_folder_id,
                    old_modified_at,
                ) in rows
                {
                    if reporter.cancelled() {
                        break;
                    }
                    done += 1;
                    reporter.report(done, total);
                    if old_folder == folder_path {
                        report.skipped += 1;
                        continue;
                    }
                    let old_path = content_path(&paths, &old_folder, &content_hash, &file_ext)?;
                    let new_path = content_path(&paths, &folder_path, &content_hash, &file_ext)?;
                    if let Err(error) = std::fs::rename(&old_path, &new_path) {
                        report.failures.push(BatchFailure {
                            image_id: Some(image_id),
                            path: old_path.to_string_lossy().into_owned(),
                            reason: format!("移动文件失败：{error}"),
                        });
                        continue;
                    }
                    match transaction.execute(
                        "UPDATE images SET folder_id = ?1, modified_at = ?2 WHERE id = ?3",
                        params![target_folder, now, image_id],
                    ) {
                        Ok(changed) if changed > 0 => {
                            report.moved += 1;
                            moves.push((image_id, old_folder_id, target_folder, old_modified_at));
                        }
                        Ok(_) => {
                            let _ = std::fs::rename(&new_path, &old_path);
                            report.failures.push(BatchFailure {
                                image_id: Some(image_id),
                                path: new_path.to_string_lossy().into_owned(),
                                reason: "图片记录已不存在".to_owned(),
                            });
                        }
                        Err(error) => {
                            if let Err(rollback) = std::fs::rename(&new_path, &old_path) {
                                log::error!(
                                    "回滚文件夹移动失败 image={image_id}: {rollback}；文件位于 {}",
                                    new_path.display()
                                );
                            }
                            report.failures.push(BatchFailure {
                                image_id: Some(image_id),
                                path: new_path.to_string_lossy().into_owned(),
                                reason: format!("更新数据库失败：{error}"),
                            });
                        }
                    }
                }
            }
            draft.moves(&moves);
            if !created_folders.is_empty() {
                draft.json(
                    &created_folders
                        .into_iter()
                        .map(|(id, path)| history::FolderRef { id, path })
                        .collect::<Vec<_>>(),
                )?;
            }
            draft.affected(report.moved);
            Ok(report)
        },
    )
}

pub fn set_trashed(database: &LibraryDb, image_ids: &[i64], trashed: bool) -> AppResult<u64> {
    set_trashed_with_id(database, image_ids, trashed).map(|(changed, _)| changed)
}

pub fn set_trashed_with_id(
    database: &LibraryDb,
    image_ids: &[i64],
    trashed: bool,
) -> AppResult<(u64, Option<i64>)> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    temp::reject_images(
        database,
        &ids,
        "临时图片不能移入回收站或恢复；关闭或切换图库时会自动清空",
    )?;
    let (kind, label) = if trashed {
        (HistoryKind::Trash, "移入回收站")
    } else {
        (HistoryKind::Restore, "恢复图片")
    };
    // 只处理仍处于目标状态的行；待释放行不参与，否则会把标记中的图片恢复成正常图片。
    let matches = if trashed {
        "deleted_at IS NULL"
    } else {
        "deleted_at IS NOT NULL"
    };
    let assigned = if trashed { "?1" } else { "NULL" };
    history::write_with_id(
        database,
        NewOperation::new(kind, label),
        |transaction, draft| {
            let now = now_ms();
            let mut triples = Vec::new();
            let mut changed = 0_u64;
            for chunk in ids.chunks(SQL_CHUNK) {
                let marks = placeholders(chunk.len());
                let mut statement = transaction.prepare(&format!(
                    "SELECT id, deleted_at, modified_at FROM images
                     WHERE id IN ({marks}) AND {matches} AND purge_operation_id IS NULL"
                ))?;
                let rows = statement
                    .query_map(params_from_iter(chunk.iter()), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);
                for (image_id, old_deleted_at, old_modified_at) in rows {
                    triples.push((image_id, old_deleted_at.unwrap_or(0), old_modified_at));
                }
                changed += transaction.execute(
                    &format!(
                        "UPDATE images SET deleted_at = {assigned}, modified_at = ?1
                         WHERE id IN ({marks}) AND {matches}
                           AND purge_operation_id IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(now))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )? as u64;
            }
            draft.triples(&triples);
            draft.affected(changed);
            Ok(changed)
        },
    )
}

/// 永久删除：整批都必须在回收站里，否则拒绝（避免绕过界面隐藏正常图片）。
/// 只写待释放标记，原图与数据库行由释放任务删除。
pub fn delete_images(
    database: &LibraryDb,
    image_ids: &[i64],
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    let ids = validate_ids_for_files(image_ids)?;
    mark_for_purge(database, Some(&ids), reporter)
}

/// 清空回收站：给全部可删行写待释放标记，文件同样等释放任务。
pub fn empty_trash(database: &LibraryDb, reporter: &dyn BatchReporter) -> AppResult<DeleteReport> {
    mark_for_purge(database, None, reporter)
}

/// 永久删除/清空回收站只写待释放标记；文件在对应记录过期或清空记录后由释放任务删除。
fn mark_for_purge(
    database: &LibraryDb,
    image_ids: Option<&[i64]>,
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    let targets = purge_targets(database, image_ids)?;
    let (kind, label) = if image_ids.is_some() {
        (HistoryKind::PermanentDelete, "永久删除")
    } else {
        (HistoryKind::EmptyTrash, "清空回收站")
    };
    if targets.is_empty() {
        return Ok(DeleteReport {
            deferred: true,
            ..Default::default()
        });
    }
    let total = targets.len();
    reporter.report(0, total);

    history::write(
        database,
        NewOperation::new(kind, label),
        |transaction, draft| {
            let mut changed = 0_u64;
            let mut done = 0_usize;
            for chunk in targets.chunks(SQL_CHUNK) {
                if reporter.cancelled() {
                    break;
                }
                let marks = placeholders(chunk.len());
                changed += transaction.execute(
                    &format!(
                        "UPDATE images SET purge_operation_id = ?1
                     WHERE id IN ({marks}) AND deleted_at IS NOT NULL
                       AND purge_operation_id IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(draft.id()))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )? as u64;
                done += chunk.len();
                reporter.report(done, total);
            }
            draft.affected(changed);
            Ok(DeleteReport {
                deleted: changed,
                files_removed: 0,
                deferred: true,
                failures: Vec::new(),
            })
        },
    )
}

/// 永久删除要求整批都在回收站（否则拒绝，避免绕过界面隐藏正常图片）；清空回收站取全部可删行。
fn purge_targets(database: &LibraryDb, image_ids: Option<&[i64]>) -> AppResult<Vec<i64>> {
    let Some(ids) = image_ids else {
        return database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id FROM images
                 WHERE deleted_at IS NOT NULL AND purge_operation_id IS NULL",
            )?;
            let ids = statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        });
    };
    let mut eligible = Vec::with_capacity(ids.len());
    database.with_reader(|connection| {
        for chunk in ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let mut statement = connection.prepare(&format!(
                "SELECT id FROM images
                 WHERE id IN ({marks}) AND deleted_at IS NOT NULL
                   AND purge_operation_id IS NULL"
            ))?;
            eligible.extend(
                statement
                    .query_map(params_from_iter(chunk.iter()), |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(())
    })?;
    if eligible.len() != ids.len() {
        return Err(AppError::message("只能永久删除回收站中的图片"));
    }
    Ok(eligible)
}

/// 释放任务：按 `purge_operation_id` 删行并删原图；每块先提交删行，再删文件。
pub fn purge_by_operation(
    database: &LibraryDb,
    root: &Path,
    operation_ids: &[i64],
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    if operation_ids.is_empty() {
        return Err(AppError::message("没有指定待释放的操作记录"));
    }
    let paths = LibraryPaths::new(root);
    let selected = database.with_reader(|connection| {
        let mut selected = Vec::new();
        for chunk in operation_ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let mut statement = connection.prepare(&format!(
                "SELECT images.id, images.content_hash, images.file_ext, folders.path
                 FROM images JOIN folders ON folders.id = images.folder_id
                 WHERE images.purge_operation_id IN ({marks})"
            ))?;
            selected.extend(
                statement
                    .query_map(params_from_iter(chunk.iter()), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(selected)
    })?;

    let mut files = Vec::with_capacity(selected.len());
    for (image_id, content_hash, file_ext, folder_path) in selected {
        match content_path(&paths, &folder_path, &content_hash, &file_ext) {
            Ok(path) => files.push((image_id, path)),
            Err(error) => log::warn!("跳过无效图片路径 image={image_id}: {error}"),
        }
    }

    let total = files.len();
    reporter.report(0, total);
    let mut report = DeleteReport::default();
    let mut done = 0_usize;

    for chunk in files.chunks(SQL_CHUNK) {
        if reporter.cancelled() {
            break;
        }
        let ids: Vec<i64> = chunk.iter().map(|(id, _)| *id).collect();
        let deleted = database.with_writer(|connection| {
            let transaction = connection.transaction()?;
            let marks = placeholders(ids.len());
            let deleted = transaction.execute(
                &format!("DELETE FROM images WHERE id IN ({marks})"),
                params_from_iter(ids.iter()),
            )?;
            transaction.commit()?;
            Ok(deleted as u64)
        })?;
        report.deleted += deleted;

        for (image_id, path) in chunk {
            if path.exists() {
                match remove_content_file(&paths.root, path) {
                    Ok(()) => report.files_removed += 1,
                    Err(error) => report.failures.push(BatchFailure {
                        image_id: Some(*image_id),
                        path: path.to_string_lossy().into_owned(),
                        reason: error.to_string(),
                    }),
                }
            }
            done += 1;
            reporter.report(done, total);
        }
    }
    Ok(report)
}

pub(crate) fn content_path(
    paths: &LibraryPaths,
    folder_path: &str,
    content_hash: &str,
    file_ext: &str,
) -> AppResult<PathBuf> {
    if content_hash.len() != 64 || !content_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::message("内容哈希无效，拒绝操作文件"));
    }
    let normalized_folder = crate::library::paths::normalize_stored_folder(folder_path)?;
    if normalized_folder != folder_path {
        return Err(AppError::message("文件夹路径未规范化，拒绝操作文件"));
    }
    let extension = normalize_content_extension(file_ext)?;
    if extension != file_ext {
        return Err(AppError::message("扩展名未规范化，拒绝操作文件"));
    }
    Ok(paths.content_file(&normalized_folder, &format!("{content_hash}.{extension}")))
}

fn ensure_inside_root(root: &Path, path: &Path) -> AppResult<()> {
    let canonical_root = root.canonicalize()?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::message("目标路径没有父目录"))?;
    let canonical_parent = parent.canonicalize()?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(AppError::message("目标路径位于图库目录之外"));
    }
    Ok(())
}

/// 目标目录必须已经存在，且 canonicalize 后仍是图库根目录或其子目录。
fn ensure_dir_inside_root(root: &Path, directory: &Path) -> AppResult<()> {
    let canonical_root = root.canonicalize()?;
    let canonical_directory = directory.canonicalize()?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(AppError::message("目标路径位于图库目录之外"));
    }
    Ok(())
}

pub(crate) fn remove_content_file(root: &Path, path: &Path) -> AppResult<()> {
    ensure_inside_root(root, path)?;
    let canonical_root = root.canonicalize()?;
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(&canonical_root) {
        return Err(AppError::message("文件位于图库目录之外"));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

fn tag_refs(database: &LibraryDb, image_id: i64) -> AppResult<Vec<TagRef>> {
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

fn tag_ids_for_names(connection: &Connection, names: &[String]) -> AppResult<Vec<i64>> {
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

fn normalize_tag_names(names: &[String]) -> AppResult<Vec<String>> {
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

fn validate_tag_name(name: &str) -> AppResult<String> {
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

fn validate_ids(values: &[i64], max: usize) -> AppResult<Vec<i64>> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        if *value <= 0 {
            return Err(AppError::message("图片 id 无效"));
        }
        if seen.insert(*value) {
            result.push(*value);
        }
    }
    if result.is_empty() {
        return Err(AppError::message("没有选择图片"));
    }
    if result.len() > max {
        return Err(AppError::message(format!("一次最多操作 {max} 张图片")));
    }
    Ok(result)
}

fn validate_ids_for_files(values: &[i64]) -> AppResult<Vec<i64>> {
    validate_ids(values, MAX_FILE_BATCH_IDS).map_err(|error| match error {
        AppError::Message(message) if message.starts_with("一次最多操作") => {
            AppError::message(format!(
                "涉及文件的批量操作一次最多 {MAX_FILE_BATCH_IDS} 张，请分批执行"
            ))
        }
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::{NoopReporter, import::ensure_folder, service};

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        root: PathBuf,
        folder: i64,
    }

    fn fixture() -> Fixture {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "元数据测试", 1).unwrap();
        let folder = session
            .db
            .with_writer(|connection| ensure_folder(connection, "人物", 1))
            .unwrap();
        Fixture {
            _temp: temp,
            session,
            root,
            folder,
        }
    }

    fn insert_image(
        fixture: &Fixture,
        seed: &str,
        extension: &str,
        score: i64,
        folder_id: i64,
    ) -> i64 {
        let hash = format!("{seed:0>64}");
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, ?2, ?3, 100, 10, 10, ?2, 0,
                        zeroblob(32), ?4, ?5, ?6, '', 1, 1)",
                    params![
                        hash,
                        extension,
                        folder_id,
                        vec![1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
                        vec![20_u8; 32],
                        score
                    ],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap()
    }

    fn write_content(
        fixture: &Fixture,
        folder_path: &str,
        hash_seed: &str,
        extension: &str,
    ) -> PathBuf {
        let hash = format!("{hash_seed:0>64}");
        let file_name = format!("{hash}.{extension}");
        let path = LibraryPaths::new(&fixture.root).content_file(folder_path, &file_name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"content").unwrap();
        path
    }

    #[test]
    fn reads_and_updates_metadata() {
        let fixture = fixture();
        let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
        assert_eq!(details.score, 1);
        assert_eq!(details.colors4.len(), 4);
        assert_eq!(details.colors4[0].weight, 4);
        assert_eq!(details.colors8.len(), 8);
        assert!(
            details
                .file_path
                .ends_with(&format!("{}.jpg", "0".repeat(63) + "a"))
        );

        update_score(&fixture.session.db, image_id, 5).unwrap();
        update_description(&fixture.session.db, image_id, "  人物速写  ").unwrap();

        let tags = set_image_tags(
            &fixture.session.db,
            image_id,
            &["速写".into(), "参考".into()],
        )
        .unwrap();
        assert_eq!(tags.len(), 2);

        let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
        assert_eq!(details.score, 5);
        assert_eq!(details.description, "人物速写");
        assert_eq!(details.description_source, "user");
        assert_eq!(details.tags.len(), 2);

        let found: i64 = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM image_search WHERE image_search MATCH '\"人物速\"'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(found, 1);
    }

    #[test]
    fn temp_metadata_changes_do_not_write_history() {
        let fixture = fixture();
        let temp_folder = fixture
            .session
            .db
            .with_reader(temp::folder)
            .unwrap()
            .unwrap()
            .id;
        let temp_image = insert_image(&fixture, "t", "jpg", 0, temp_folder);
        let normal_image = insert_image(&fixture, "n", "jpg", 0, fixture.folder);

        update_score(&fixture.session.db, temp_image, 5).unwrap();
        update_description(&fixture.session.db, temp_image, "临时描述").unwrap();
        set_image_tags(&fixture.session.db, temp_image, &["临时".into()]).unwrap();
        assert!(history::list(&fixture.session.db).unwrap().is_empty());

        set_scores(&fixture.session.db, &[temp_image, normal_image], 4).unwrap();
        let entries = history::list(&fixture.session.db).unwrap();
        assert_eq!(entries.len(), 1);

        let details = image_details(&fixture.session.db, &fixture.root, temp_image).unwrap();
        assert_eq!(details.score, 4);
        assert_eq!(details.description, "临时描述");
    }

    #[test]
    fn edits_tags_in_batches_and_renames() {
        let fixture = fixture();
        let first = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let second = insert_image(&fixture, "b", "png", 1, fixture.folder);

        assert_eq!(
            add_tags(
                &fixture.session.db,
                &[first, second],
                &["速写".into(), "练习".into()]
            )
            .unwrap(),
            4
        );
        assert_eq!(
            remove_tags(&fixture.session.db, &[first], &["练习".into()]).unwrap(),
            1
        );
        assert_eq!(
            image_details(&fixture.session.db, &fixture.root, first)
                .unwrap()
                .tags
                .len(),
            1
        );

        let tag_id = image_details(&fixture.session.db, &fixture.root, first)
            .unwrap()
            .tags[0]
            .id;
        rename_tag(&fixture.session.db, tag_id, "草稿").unwrap();
        let names: Vec<String> = image_details(&fixture.session.db, &fixture.root, second)
            .unwrap()
            .tags
            .into_iter()
            .map(|tag| tag.name)
            .collect();
        assert!(names.contains(&"草稿".to_owned()));

        let affected = delete_tag(&fixture.session.db, tag_id).unwrap();
        assert_eq!(affected, 2);
        assert_eq!(
            image_details(&fixture.session.db, &fixture.root, first)
                .unwrap()
                .tags
                .len(),
            0
        );
    }

    #[test]
    fn moves_files_between_folders_and_rolls_back_failures() {
        let fixture = fixture();
        let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let old_path = write_content(&fixture, "人物", "a", "jpg");

        let report = move_images(
            &fixture.session.db,
            &fixture.root,
            &[image_id],
            "风景/子目录",
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.moved, 1);
        assert!(report.failures.is_empty());
        assert!(!old_path.exists());
        let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
        assert_eq!(details.folder_path, "风景/子目录");
        assert!(Path::new(&details.file_path).exists());

        let report = move_images(
            &fixture.session.db,
            &fixture.root,
            &[image_id],
            "风景/子目录",
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.skipped, 1);

        // 目标位置被目录占用时，重命名失败；数据库仍指向原文件夹。
        let blocked = insert_image(&fixture, "b", "png", 1, fixture.folder);
        let blocked_path = write_content(&fixture, "人物", "b", "png");
        let target = LibraryPaths::new(&fixture.root)
            .content_file("风景/子目录", &format!("{}.png", "0".repeat(63) + "b"));
        fs::create_dir_all(&target).unwrap();
        let report = move_images(
            &fixture.session.db,
            &fixture.root,
            &[blocked],
            "风景/子目录",
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.moved, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(blocked_path.exists());
        assert_eq!(
            image_details(&fixture.session.db, &fixture.root, blocked)
                .unwrap()
                .folder_path,
            "人物"
        );
    }

    #[test]
    fn trashes_restores_and_permanently_deletes() {
        let fixture = fixture();
        let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let path = write_content(&fixture, "人物", "a", "jpg");
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute("INSERT INTO tags(name) VALUES ('速写')", [])?;
                connection.execute(
                    "INSERT INTO collections(id, name, sort_order, created_at, updated_at)
                     VALUES (1, '集合', 0, 1, 1)",
                    [],
                )?;
                connection.execute(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (1, ?1, 0)",
                    [image_id],
                )?;
                connection.execute(
                    "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name, pack_offset, pack_length, updated_at)
                     VALUES (?1, 'embedding', 'm', 3, 'f.bin', 0, 12, 1)",
                    [image_id],
                )?;
                connection.execute(
                    "INSERT INTO image_tags(image_id, tag_id)
                     SELECT ?1, id FROM tags WHERE name = '速写'",
                    [image_id],
                )?;
                Ok(())
            })
            .unwrap();

        assert_eq!(
            set_trashed(&fixture.session.db, &[image_id], true).unwrap(),
            1
        );
        let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
        assert!(details.deleted_at.is_some());
        assert!(path.exists());
        assert_eq!(
            set_trashed(&fixture.session.db, &[image_id], false).unwrap(),
            1
        );
        assert!(
            image_details(&fixture.session.db, &fixture.root, image_id)
                .unwrap()
                .deleted_at
                .is_none()
        );
        // 永久删除只接受回收站里的图片，避免绕过界面隐藏正常图片。
        assert!(delete_images(&fixture.session.db, &[image_id], &NoopReporter).is_err());
        set_trashed(&fixture.session.db, &[image_id], true).unwrap();

        let report = delete_images(&fixture.session.db, &[image_id], &NoopReporter).unwrap();
        assert!(report.deferred);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.files_removed, 0);
        assert!(path.exists());
        assert!(
            image_details(&fixture.session.db, &fixture.root, image_id)
                .unwrap()
                .deleted_at
                .is_some()
        );

        let operation = history::list(&fixture.session.db)
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == HistoryKind::PermanentDelete)
            .unwrap();
        let report = purge_by_operation(
            &fixture.session.db,
            &fixture.root,
            &[operation.id],
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(report.files_removed, 1);
        assert!(!path.exists());
        assert!(image_details(&fixture.session.db, &fixture.root, image_id).is_err());

        let leftovers: (i64, i64, i64, i64) = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok((
                    connection
                        .query_row("SELECT count(*) FROM image_tags", [], |row| row.get(0))?,
                    connection.query_row("SELECT count(*) FROM collection_items", [], |row| {
                        row.get(0)
                    })?,
                    connection
                        .query_row("SELECT count(*) FROM feature_blobs", [], |row| row.get(0))?,
                    connection
                        .query_row("SELECT count(*) FROM image_search", [], |row| row.get(0))?,
                ))
            })
            .unwrap();
        assert_eq!(leftovers, (0, 0, 0, 0));
    }

    #[test]
    fn empties_trash_only() {
        let fixture = fixture();
        let active = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let trashed = insert_image(&fixture, "b", "jpg", 1, fixture.folder);
        write_content(&fixture, "人物", "a", "jpg");
        let trashed_path = write_content(&fixture, "人物", "b", "jpg");
        set_trashed(&fixture.session.db, &[trashed], true).unwrap();

        let report = empty_trash(&fixture.session.db, &NoopReporter).unwrap();
        assert!(report.deferred);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.files_removed, 0);
        assert!(trashed_path.exists());
        assert!(image_details(&fixture.session.db, &fixture.root, active).is_ok());
        assert!(
            image_details(&fixture.session.db, &fixture.root, trashed)
                .unwrap()
                .deleted_at
                .is_some()
        );

        let operation = history::list(&fixture.session.db)
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == HistoryKind::EmptyTrash)
            .unwrap();
        purge_by_operation(
            &fixture.session.db,
            &fixture.root,
            &[operation.id],
            &NoopReporter,
        )
        .unwrap();
        assert!(!trashed_path.exists());
        assert!(image_details(&fixture.session.db, &fixture.root, trashed).is_err());
    }

    #[test]
    fn rejects_invalid_inputs() {
        let fixture = fixture();
        let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        assert!(update_score(&fixture.session.db, image_id, 6).is_err());
        assert!(update_description(&fixture.session.db, image_id, &"x".repeat(50_001)).is_err());
        assert!(rename_tag(&fixture.session.db, 1, "").is_err());
        assert!(
            move_images(
                &fixture.session.db,
                &fixture.root,
                &[],
                "目标",
                &NoopReporter
            )
            .is_err()
        );
        assert!(
            move_images(
                &fixture.session.db,
                &fixture.root,
                &[image_id],
                "../外面",
                &NoopReporter
            )
            .is_err()
        );
    }
}

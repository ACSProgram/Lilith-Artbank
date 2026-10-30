use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use rusqlite::{Connection, OptionalExtension, params_from_iter};
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    library::{db::LibraryDb, paths::LibraryPaths, placeholders},
};

use super::{MAX_BATCH_IDS, SQL_CHUNK, validate_ids};

const SELECTION_TAG_CHUNK: usize = 4_000;
/// 关系数超过“选中数 × 该系数”时，分块探测比全表扫描便宜。阈值校准见
/// `docs/guides/performance.md` 的“选中标签聚合”。
const SELECTION_TAG_SCAN_FACTOR: i64 = 16;
const MAX_STATE_IDS: usize = 1_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageState {
    pub id: i64,
    pub deleted_at: Option<i64>,
}

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

/// 选中集合内某个标签的拥有数；不是全库数量。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionTag {
    pub id: i64,
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionTags {
    /// 去重后的选中图片数，用于判断“全部都有”。
    pub total: u64,
    pub tags: Vec<SelectionTag>,
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
    /// 所在文件夹已整目录移入回收站；这类图片不提供单张恢复/永久删除。
    pub folder_trashed: bool,
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
                        images.colors4, images.colors8, folders.trashed_at IS NOT NULL
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
                        folder_trashed: row.get::<_, i64>(22)? != 0,
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

/// 按 id 批量取状态；已不存在的 id 不返回。外部变化后复核选择、相似参考与查看器。
pub fn image_states(database: &LibraryDb, image_ids: &[i64]) -> AppResult<Vec<ImageState>> {
    let mut unique: Vec<i64> = image_ids.iter().copied().filter(|id| *id > 0).collect();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() > MAX_STATE_IDS {
        return Err(AppError::message(format!(
            "一次最多查询 {MAX_STATE_IDS} 张图片的状态"
        )));
    }
    if unique.is_empty() {
        return Ok(Vec::new());
    }
    database.with_reader(|connection| {
        let mut states = Vec::with_capacity(unique.len());
        for chunk in unique.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let mut statement = connection.prepare(&format!(
                "SELECT id, deleted_at FROM images WHERE id IN ({marks}) ORDER BY id"
            ))?;
            states.extend(
                statement
                    .query_map(params_from_iter(chunk.iter()), |row| {
                        Ok(ImageState {
                            id: row.get(0)?,
                            deleted_at: row.get(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(states)
    })
}

/// 选中集合内各标签的拥有数。小选择按 id 分块探测；大选择扫描关系表并用集合过滤。
pub fn selection_tags(database: &LibraryDb, image_ids: &[i64]) -> AppResult<SelectionTags> {
    if image_ids.is_empty() {
        return Ok(SelectionTags {
            total: 0,
            tags: Vec::new(),
        });
    }
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    let total = ids.len() as u64;
    database.with_reader(|connection| {
        let relation_count: i64 =
            connection.query_row("SELECT count(*) FROM image_tags", [], |row| row.get(0))?;
        let counts = if prefer_chunked_probe(relation_count, ids.len()) {
            count_selection_tags_by_chunk(connection, &ids)?
        } else {
            count_selection_tags_by_scan(connection, &ids)?
        };
        if counts.is_empty() {
            return Ok(SelectionTags {
                total,
                tags: Vec::new(),
            });
        }

        let mut names = tag_names(connection, counts.keys().copied())?;
        let mut tags: Vec<SelectionTag> = counts
            .into_iter()
            .filter_map(|(id, count)| {
                names
                    .remove(&id)
                    .map(|name| SelectionTag { id, name, count })
            })
            .collect();
        tags.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(SelectionTags { total, tags })
    })
}

/// 关系表远大于选择时，逐块主键探测比整表扫描便宜。
pub(super) fn prefer_chunked_probe(relation_count: i64, selection: usize) -> bool {
    relation_count > (selection as i64).saturating_mul(SELECTION_TAG_SCAN_FACTOR)
}

/// 小块主键探测：按 `SELECTION_TAG_CHUNK` 分块聚合，再在内存里合并计数。
fn count_selection_tags_by_chunk(
    connection: &Connection,
    ids: &[i64],
) -> AppResult<HashMap<i64, u64>> {
    let mut counts: HashMap<i64, u64> = HashMap::new();
    for chunk in ids.chunks(SELECTION_TAG_CHUNK) {
        let marks = placeholders(chunk.len());
        let mut statement = connection.prepare(&format!(
            "SELECT tag_id, count(*) FROM image_tags
             WHERE image_id IN ({marks}) GROUP BY tag_id"
        ))?;
        let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (tag_id, count) = row?;
            *counts.entry(tag_id).or_insert(0) += count as u64;
        }
    }
    Ok(counts)
}

/// 关系表一次覆盖索引扫描：SQL 侧不按选择过滤，只在 Rust 侧统计选中的图片。
fn count_selection_tags_by_scan(
    connection: &Connection,
    ids: &[i64],
) -> AppResult<HashMap<i64, u64>> {
    let selected: HashSet<i64> = ids.iter().copied().collect();
    let mut counts: HashMap<i64, u64> = HashMap::new();
    let mut statement = connection.prepare("SELECT image_id, tag_id FROM image_tags")?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
    for row in rows {
        let (image_id, tag_id) = row?;
        if selected.contains(&image_id) {
            *counts.entry(tag_id).or_insert(0) += 1;
        }
    }
    Ok(counts)
}

/// 按 id 批量取标签名；关系表里引用了已删除标签的 id 会缺席。
fn tag_names(
    connection: &Connection,
    tag_ids: impl Iterator<Item = i64>,
) -> AppResult<HashMap<i64, String>> {
    let tag_ids: Vec<i64> = tag_ids.collect();
    let mut names: HashMap<i64, String> = HashMap::new();
    for chunk in tag_ids.chunks(SQL_CHUNK) {
        let marks = placeholders(chunk.len());
        let mut statement =
            connection.prepare(&format!("SELECT id, name FROM tags WHERE id IN ({marks})"))?;
        let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, name) = row?;
            names.insert(id, name);
        }
    }
    Ok(names)
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

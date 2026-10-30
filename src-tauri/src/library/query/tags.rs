use std::collections::HashMap;

use rusqlite::{Connection, params_from_iter, types::Value};

use crate::app::error::AppResult;
use crate::library::{db::LibraryDb, paths::escape_like, temp};

use super::{MAX_TAGS, MAX_TAGS_WITH_EMPTY, TagInfo};

pub fn list_tags(database: &LibraryDb, limit: Option<u32>) -> AppResult<Vec<TagInfo>> {
    list_tags_page(database, None, limit.unwrap_or(MAX_TAGS), 0)
}

/// 与 [`list_tags`] 相同，但可以带上图片数为 0 的标签（标签管理器要用）；
/// 默认不带的旧行为保持不变。
pub fn list_tags_with_empty(
    database: &LibraryDb,
    limit: Option<u32>,
    include_empty: bool,
) -> AppResult<Vec<TagInfo>> {
    if !include_empty {
        return list_tags(database, limit);
    }
    list_tags_page_with(database, None, limit.unwrap_or(MAX_TAGS), 0, true)
}

pub fn search_tags(
    database: &LibraryDb,
    query: &str,
    limit: Option<u32>,
) -> AppResult<Vec<TagInfo>> {
    search_tags_page(database, query, limit.unwrap_or(50), 0)
}

pub fn search_tags_with_empty(
    database: &LibraryDb,
    query: &str,
    limit: Option<u32>,
    include_empty: bool,
) -> AppResult<Vec<TagInfo>> {
    if !include_empty {
        return search_tags(database, query, limit);
    }
    let needle = query.trim();
    if needle.is_empty() {
        return list_tags_with_empty(database, limit, true);
    }
    list_tags_page_with(
        database,
        Some(needle),
        limit.unwrap_or(50).clamp(1, MAX_TAGS_WITH_EMPTY),
        0,
        true,
    )
}

/// 按名称模糊匹配标签；`limit` 最高 200，供界面自动补全使用。
pub fn search_tags_page(
    database: &LibraryDb,
    query: &str,
    limit: u32,
    offset: u32,
) -> AppResult<Vec<TagInfo>> {
    let needle = query.trim();
    if needle.is_empty() {
        return list_tags_page(database, None, limit.min(MAX_TAGS), offset);
    }
    list_tags_page(database, Some(needle), limit.clamp(1, 200), offset)
}

/// 分页取标签；`search` 为空时按图片数降序，否则按名称 `LIKE` 过滤。
///
/// 临时图片不计入标签；标签本身仍保留，标签清空临时区后自然消失。
///
/// 默认形状从 `image_tags` 覆盖扫描聚合计数，再回连 `tags`：`image_tags` 有外键级联，
/// 不存在指向已删图片的孤儿关系行，所以按关系行计数与旧 SQL 的“回表过滤”等价。
/// 回收站与临时区里的关系行用有界减法扣掉；这两类图片太多（例如刚清空回收站前）时
/// 退回逐张回表的旧 SQL，避免为减法建一张几十万条 id 的临时集合。
pub fn list_tags_page(
    database: &LibraryDb,
    search: Option<&str>,
    limit: u32,
    offset: u32,
) -> AppResult<Vec<TagInfo>> {
    list_tags_page_with(database, search, limit, offset, false)
}

/// 带 0 张标签的版本：走一条独立查询，不做「减法」那条快路径。
///
/// 只有标签管理器会用，不在任何滚动/翻页热路径上；556 个标签逐个子查询也是毫秒级。
pub fn list_tags_page_with(
    database: &LibraryDb,
    search: Option<&str>,
    limit: u32,
    offset: u32,
    include_empty: bool,
) -> AppResult<Vec<TagInfo>> {
    let search = search.map(str::trim).filter(|text| !text.is_empty());
    database.with_reader(|connection| {
        if include_empty {
            return list_tags_with_empty_in(connection, search, limit, offset);
        }
        list_tags_page_in(connection, search, limit, offset, MAX_TAG_DEAD_IMAGES)
    })
}

/// 0 张标签也列出来：计数子查询与 [`count_tags`] 同一口径（排除回收站与临时区）。
fn list_tags_with_empty_in(
    connection: &Connection,
    search: Option<&str>,
    limit: u32,
    offset: u32,
) -> AppResult<Vec<TagInfo>> {
    let limit = limit.clamp(1, MAX_TAGS_WITH_EMPTY);
    let mut statement = connection.prepare(&format!(
        "SELECT tags.id, tags.name,
                (SELECT count(*) FROM image_tags
                 JOIN images ON images.id = image_tags.image_id
                 WHERE image_tags.tag_id = tags.id
                   AND images.deleted_at IS NULL
                   AND {exclude}) AS image_count
         FROM tags
         {name_filter}
         ORDER BY image_count DESC, tags.name
         LIMIT ?1 OFFSET ?2",
        exclude = temp::EXCLUDE_SQL,
        name_filter = if search.is_some() {
            "WHERE tags.name LIKE ?3 ESCAPE '\\'"
        } else {
            ""
        },
    ))?;
    let mut params = vec![
        Value::Integer(i64::from(limit)),
        Value::Integer(i64::from(offset)),
    ];
    if let Some(needle) = search {
        params.push(Value::Text(format!("%{}%", escape_like(needle))));
    }
    let rows = statement.query_map(params_from_iter(params.iter()), |row| {
        Ok(TagInfo {
            id: row.get(0)?,
            name: row.get(1)?,
            image_count: row.get::<_, i64>(2)?.max(0) as u64,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 回收站与临时区的图片数超过这个量时，减法不再划算，改回旧形状。
pub(super) const MAX_TAG_DEAD_IMAGES: u64 = 50_000;

pub(super) fn list_tags_page_in(
    connection: &Connection,
    search: Option<&str>,
    limit: u32,
    offset: u32,
    max_dead_images: u64,
) -> AppResult<Vec<TagInfo>> {
    let limit = limit.clamp(1, MAX_TAGS);
    let dead = dead_tag_counts(connection, max_dead_images)?;
    let Some(dead) = dead else {
        return list_tags_page_legacy(connection, search, limit, offset);
    };

    let mut names = connection.prepare(match search {
        Some(_) => "SELECT id, name FROM tags WHERE name LIKE ?1 ESCAPE '\\'",
        None => "SELECT id, name FROM tags",
    })?;
    let mut params = Vec::new();
    if let Some(needle) = search {
        params.push(Value::Text(format!("%{}%", escape_like(needle))));
    }
    let names = names
        .query_map(params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut counts: HashMap<i64, u64> = connection
        .prepare("SELECT tag_id, count(*) FROM image_tags GROUP BY tag_id")?
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as u64))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    for (tag_id, count) in dead {
        if let Some(total) = counts.get_mut(&tag_id) {
            *total = total.saturating_sub(count);
        }
    }

    let mut tags: Vec<TagInfo> = names
        .into_iter()
        .filter_map(|(id, name)| {
            let image_count = counts.get(&id).copied().unwrap_or(0);
            (image_count > 0).then_some(TagInfo {
                id,
                name,
                image_count,
            })
        })
        .collect();
    tags.sort_by(|left, right| {
        right
            .image_count
            .cmp(&left.image_count)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(tags
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect())
}

/// 回收站与临时区里的关系行按标签计数；这两类图片数超过阈值时返回 `None`。
///
/// 阈值只看图片数，同一张图既在回收站又在临时区时会重复计数——那只是让回退更早
/// 发生，减法本身仍按 id 集合去重，不影响结果。
fn dead_tag_counts(
    connection: &Connection,
    max_dead_images: u64,
) -> AppResult<Option<Vec<(i64, u64)>>> {
    let trash: i64 = connection.query_row(
        "SELECT count(*) FROM images WHERE deleted_at IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    let temp: i64 = connection.query_row(
        "SELECT count(*) FROM images WHERE folder_id = COALESCE(
             (SELECT temp_folders.id FROM folders temp_folders WHERE temp_folders.kind = 'temp'), -1)",
        [],
        |row| row.get(0),
    )?;
    let dead_images = trash.max(0) as u64 + temp.max(0) as u64;
    if dead_images == 0 {
        return Ok(Some(Vec::new()));
    }
    if dead_images > max_dead_images {
        return Ok(None);
    }
    let rows = connection
        .prepare(
            "SELECT tag_id, count(*) FROM image_tags
             WHERE image_id IN (
                 SELECT id FROM images
                 WHERE deleted_at IS NOT NULL
                    OR folder_id = COALESCE(
                        (SELECT temp_folders.id FROM folders temp_folders
                         WHERE temp_folders.kind = 'temp'), -1)
             )
             GROUP BY tag_id",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as u64))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(rows))
}

/// 旧形状：`tags → image_tags → images` 逐张回表；死亡关系过多时使用。
pub(super) fn list_tags_page_legacy(
    connection: &Connection,
    search: Option<&str>,
    limit: u32,
    offset: u32,
) -> AppResult<Vec<TagInfo>> {
    let mut statement = connection.prepare(&format!(
        "SELECT tags.id, tags.name, count(images.id)
         FROM tags
         JOIN image_tags ON image_tags.tag_id = tags.id
         JOIN images ON images.id = image_tags.image_id AND images.deleted_at IS NULL
         WHERE {exclude}{name_filter}
         GROUP BY tags.id
         ORDER BY count(images.id) DESC, tags.name
         LIMIT ?1 OFFSET ?2",
        exclude = temp::EXCLUDE_SQL,
        name_filter = if search.is_some() {
            " AND tags.name LIKE ?3 ESCAPE '\\'"
        } else {
            ""
        },
    ))?;
    let mut params = vec![
        Value::Integer(i64::from(limit)),
        Value::Integer(i64::from(offset)),
    ];
    if let Some(needle) = search {
        params.push(Value::Text(format!("%{}%", escape_like(needle))));
    }
    let rows = statement.query_map(params_from_iter(params.iter()), |row| {
        Ok(TagInfo {
            id: row.get(0)?,
            name: row.get(1)?,
            image_count: row.get::<_, i64>(2)?.max(0) as u64,
        })
    })?;
    let tags = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(tags)
}

/// 基准专用：在真实库规模上对比新旧形状（`perf_metadata_200k`）。
#[cfg(test)]
pub(crate) fn list_tags_page_legacy_bench(
    database: &LibraryDb,
    limit: u32,
) -> AppResult<Vec<TagInfo>> {
    database.with_reader(|connection| list_tags_page_legacy(connection, None, limit, 0))
}

/// 与 [`list_tags_page`] 同口径的命中总数，用于判断还有没有下一页。
pub fn count_tags(database: &LibraryDb, search: Option<&str>) -> AppResult<u64> {
    let search = search.map(str::trim).filter(|text| !text.is_empty());
    database.with_reader(|connection| {
        let mut statement = connection.prepare(&format!(
            "SELECT count(*) FROM tags
             WHERE EXISTS (
                 SELECT 1 FROM image_tags
                 JOIN images ON images.id = image_tags.image_id
                 WHERE image_tags.tag_id = tags.id
                   AND images.deleted_at IS NULL
                   AND {exclude}
             ){name_filter}",
            exclude = temp::EXCLUDE_SQL,
            name_filter = if search.is_some() {
                " AND tags.name LIKE ?1 ESCAPE '\\'"
            } else {
                ""
            },
        ))?;
        let mut params = Vec::new();
        if let Some(needle) = search {
            params.push(Value::Text(format!("%{}%", escape_like(needle))));
        }
        let total: i64 = statement.query_row(params_from_iter(params.iter()), |row| row.get(0))?;
        Ok(total.max(0) as u64)
    })
}

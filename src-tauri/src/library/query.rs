use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, params_from_iter, types::Value};
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::{db::LibraryDb, paths::normalize_content_extension},
};

pub const DEFAULT_LIMIT: u32 = 150;
pub const MAX_LIMIT: u32 = 300;
pub const MAX_TAGS: u32 = 500;
const MAX_IDS: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LibraryView {
    #[default]
    All,
    Trash,
    Collection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SortKey {
    #[default]
    ImportedAt,
    Score,
    Size,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::ImportedAt,
            descending: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TagMode {
    #[default]
    Any,
    All,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryFilters {
    pub folder_ids: Vec<i64>,
    pub extensions: Vec<String>,
    pub scores: Vec<i64>,
    pub tag_ids: Vec<i64>,
    pub tag_mode: TagMode,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryQuery {
    pub view: LibraryView,
    pub collection_id: Option<i64>,
    pub search: String,
    pub filters: LibraryFilters,
    pub sort: SortSpec,
    pub cursor: Option<QueryCursor>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueryCursor {
    pub sort_key: String,
    pub id: i64,
}

impl QueryCursor {
    // 游标排序值是排序列的原始整数；用字符串跨 IPC 传递，避免前端数值精度问题。
    fn sort_value(&self) -> AppResult<i64> {
        self.sort_key
            .parse()
            .map_err(|_| AppError::message("游标排序值无效"))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRow {
    pub id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub folder_id: i64,
    pub folder_path: String,
    pub file_path: String,
    pub file_size: i64,
    pub width: i64,
    pub height: i64,
    pub format: String,
    pub has_alpha: bool,
    pub score: i64,
    pub imported_at: i64,
    pub modified_at: i64,
    pub thumb_version: i64,
    pub thumb_width: i64,
    pub thumb_height: i64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePage {
    pub items: Vec<ImageRow>,
    pub next_cursor: Option<QueryCursor>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderCount {
    pub folder_id: i64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueCount {
    pub value: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagCount {
    pub tag_id: i64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterCounts {
    pub total: u64,
    pub folders: Vec<FolderCount>,
    pub extensions: Vec<ValueCount>,
    pub scores: Vec<ValueCount>,
    pub tags: Vec<TagCount>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderInfo {
    pub id: i64,
    pub path: String,
    pub image_count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagInfo {
    pub id: i64,
    pub name: String,
    pub image_count: u64,
}

pub fn query_page(database: &LibraryDb, root: &Path, query: LibraryQuery) -> AppResult<ImagePage> {
    let (query, limit) = normalize(query)?;
    database.with_reader(|connection| {
        let mut filters = FilterSet::new();
        filters.push_view(query.view);
        filters.push_filters(&query.filters, None);

        let sort_column = sort_column(query.sort.key);
        let direction = if query.sort.descending { "DESC" } else { "ASC" };
        if let Some(cursor) = &query.cursor {
            let sort_key = cursor.sort_value()?;
            let operator = if query.sort.descending { "<" } else { ">" };
            filters
                .clauses
                .push(format!("({sort_column}, images.id) {operator} (?, ?)"));
            filters.params.push(Value::Integer(sort_key));
            filters.params.push(Value::Integer(cursor.id));
        }

        // 末尾多选一次排序列：游标键直接从行里读，不必再按排序键映射列下标。
        let sql = format!(
            "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                    folders.path, images.file_size, images.width, images.height,
                    images.format, images.has_alpha, images.score, images.imported_at,
                    images.modified_at, images.thumb_version, images.thumb_width,
                    images.thumb_height, {sort_column}
             FROM images
             JOIN folders ON folders.id = images.folder_id
             WHERE {}
             ORDER BY {sort_column} {direction}, images.id {direction}
             LIMIT ?",
            filters.clauses.join(" AND ")
        );
        filters.params.push(Value::Integer(i64::from(limit)));

        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(filters.params.iter()), |row| {
            let content_hash: String = row.get(1)?;
            let file_ext: String = row.get(2)?;
            let folder_path: String = row.get(4)?;
            let file_name = format!("{content_hash}.{file_ext}");
            let file_path = if folder_path.is_empty() {
                root.join(&file_name)
            } else {
                root.join(folder_path.replace('/', std::path::MAIN_SEPARATOR_STR))
                    .join(&file_name)
            };
            Ok((
                ImageRow {
                    id: row.get(0)?,
                    content_hash,
                    file_ext,
                    folder_id: row.get(3)?,
                    folder_path,
                    file_path: file_path.to_string_lossy().into_owned(),
                    file_size: row.get(5)?,
                    width: row.get(6)?,
                    height: row.get(7)?,
                    format: row.get(8)?,
                    has_alpha: row.get::<_, i64>(9)? != 0,
                    score: row.get(10)?,
                    imported_at: row.get(11)?,
                    modified_at: row.get(12)?,
                    thumb_version: row.get(13)?,
                    thumb_width: row.get(14)?,
                    thumb_height: row.get(15)?,
                    tags: Vec::new(),
                },
                row.get::<_, i64>(16)?,
            ))
        })?;

        let rows_with_keys = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let next_cursor = if rows_with_keys.len() == limit as usize {
            rows_with_keys.last().map(|(item, sort_key)| QueryCursor {
                sort_key: sort_key.to_string(),
                id: item.id,
            })
        } else {
            None
        };

        let mut items: Vec<ImageRow> = rows_with_keys.into_iter().map(|(item, _)| item).collect();
        attach_tags(connection, &mut items)?;
        Ok(ImagePage { items, next_cursor })
    })
}

pub fn query_ids(database: &LibraryDb, query: LibraryQuery) -> AppResult<Vec<i64>> {
    let (mut query, _) = normalize(query)?;
    query.cursor = None;
    database.with_reader(|connection| {
        let mut filters = FilterSet::new();
        filters.push_view(query.view);
        filters.push_filters(&query.filters, None);
        let direction = if query.sort.descending { "DESC" } else { "ASC" };
        let sql = format!(
            "SELECT images.id FROM images
             WHERE {}
             ORDER BY {} {direction}, images.id {direction}
             LIMIT ?",
            filters.clauses.join(" AND "),
            sort_column(query.sort.key),
        );
        filters.params.push(Value::Integer(MAX_IDS as i64 + 1));

        let mut statement = connection.prepare(&sql)?;
        let ids = statement
            .query_map(params_from_iter(filters.params.iter()), |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        if ids.len() > MAX_IDS {
            return Err(AppError::message("当前筛选结果超过 100 万张，无法一次全选"));
        }
        Ok(ids)
    })
}

pub fn filter_counts(database: &LibraryDb, query: LibraryQuery) -> AppResult<FilterCounts> {
    let (query, _) = normalize(query)?;
    database.with_reader(|connection| build_filter_counts(connection, &query))
}

pub fn list_folders(database: &LibraryDb) -> AppResult<Vec<FolderInfo>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT folders.id, folders.path, count(images.id)
             FROM folders
             LEFT JOIN images ON images.folder_id = folders.id AND images.deleted_at IS NULL
             GROUP BY folders.id
             ORDER BY folders.path",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(FolderInfo {
                id: row.get(0)?,
                path: row.get(1)?,
                image_count: row.get::<_, i64>(2)?.max(0) as u64,
            })
        })?;
        let folders = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(folders)
    })
}

pub fn list_tags(database: &LibraryDb, limit: Option<u32>) -> AppResult<Vec<TagInfo>> {
    let limit = limit.unwrap_or(MAX_TAGS).clamp(1, MAX_TAGS);
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT tags.id, tags.name, count(images.id)
             FROM tags
             JOIN image_tags ON image_tags.tag_id = tags.id
             JOIN images ON images.id = image_tags.image_id AND images.deleted_at IS NULL
             GROUP BY tags.id
             ORDER BY count(images.id) DESC, tags.name
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(TagInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                image_count: row.get::<_, i64>(2)?.max(0) as u64,
            })
        })?;
        let tags = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(tags)
    })
}

fn normalize(mut query: LibraryQuery) -> AppResult<(LibraryQuery, u32)> {
    if !query.search.trim().is_empty() {
        return Err(AppError::message("文本搜索将在 Stage 3 提供"));
    }
    if query.view == LibraryView::Collection || query.collection_id.is_some() {
        return Err(AppError::message("集合视图将在 Stage 3 提供"));
    }

    query.filters.folder_ids = dedupe_ids(query.filters.folder_ids, 256, "文件夹")?;
    query.filters.tag_ids = dedupe_ids(query.filters.tag_ids, 64, "标签")?;

    let mut extensions = Vec::new();
    let mut seen = HashSet::new();
    for value in query.filters.extensions {
        let extension = normalize_content_extension(&value)?;
        if seen.insert(extension.clone()) {
            extensions.push(extension);
        }
        if extensions.len() > 32 {
            return Err(AppError::message("扩展名筛选最多 32 项"));
        }
    }
    query.filters.extensions = extensions;

    let mut scores = Vec::new();
    let mut seen = HashSet::new();
    for score in query.filters.scores {
        if !(0..=5).contains(&score) {
            return Err(AppError::message("评分筛选必须在 0-5 之间"));
        }
        if seen.insert(score) {
            scores.push(score);
        }
    }
    query.filters.scores = scores;

    if let Some(cursor) = &query.cursor {
        cursor.sort_value()?;
        if cursor.id <= 0 {
            return Err(AppError::message("游标图片 id 无效"));
        }
    }

    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    Ok((query, limit))
}

fn dedupe_ids(values: Vec<i64>, max: usize, label: &str) -> AppResult<Vec<i64>> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        if value <= 0 {
            return Err(AppError::message(format!("{label} id 无效")));
        }
        if seen.insert(value) {
            result.push(value);
        }
    }
    if result.len() > max {
        return Err(AppError::message(format!("{label}筛选最多 {max} 项")));
    }
    Ok(result)
}

fn sort_column(key: SortKey) -> &'static str {
    match key {
        SortKey::ImportedAt => "images.imported_at",
        SortKey::Score => "images.score",
        SortKey::Size => "images.file_size",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Facet {
    Folders,
    Extensions,
    Scores,
    Tags,
}

struct FilterSet {
    clauses: Vec<String>,
    params: Vec<Value>,
}

impl FilterSet {
    fn new() -> Self {
        Self {
            clauses: Vec::new(),
            params: Vec::new(),
        }
    }

    fn push_view(&mut self, view: LibraryView) {
        // `Collection` 在 `normalize` 里被拒绝；S3 接入后这里再追加集合 JOIN。
        self.clauses.push(
            if view == LibraryView::Trash {
                "images.deleted_at IS NOT NULL"
            } else {
                "images.deleted_at IS NULL"
            }
            .to_owned(),
        );
    }

    fn push_filters(&mut self, filters: &LibraryFilters, exclude: Option<Facet>) {
        if exclude != Some(Facet::Folders) && !filters.folder_ids.is_empty() {
            let marks = placeholders(filters.folder_ids.len());
            self.clauses.push(format!("images.folder_id IN ({marks})"));
            self.params
                .extend(filters.folder_ids.iter().map(|id| Value::Integer(*id)));
        }

        if exclude != Some(Facet::Extensions) && !filters.extensions.is_empty() {
            let marks = placeholders(filters.extensions.len());
            self.clauses.push(format!("images.file_ext IN ({marks})"));
            self.params.extend(
                filters
                    .extensions
                    .iter()
                    .map(|value| Value::Text(value.clone())),
            );
        }

        if exclude != Some(Facet::Scores) && !filters.scores.is_empty() {
            let marks = placeholders(filters.scores.len());
            self.clauses.push(format!("images.score IN ({marks})"));
            self.params
                .extend(filters.scores.iter().map(|score| Value::Integer(*score)));
        }

        if exclude != Some(Facet::Tags) && !filters.tag_ids.is_empty() {
            match filters.tag_mode {
                TagMode::Any => {
                    let marks = placeholders(filters.tag_ids.len());
                    self.clauses.push(format!(
                        "EXISTS (SELECT 1 FROM image_tags
                         WHERE image_tags.image_id = images.id
                           AND image_tags.tag_id IN ({marks}))"
                    ));
                    self.params
                        .extend(filters.tag_ids.iter().map(|tag_id| Value::Integer(*tag_id)));
                }
                TagMode::All => {
                    for tag_id in &filters.tag_ids {
                        self.clauses.push(
                            "EXISTS (SELECT 1 FROM image_tags
                             WHERE image_tags.image_id = images.id AND image_tags.tag_id = ?)"
                                .to_owned(),
                        );
                        self.params.push(Value::Integer(*tag_id));
                    }
                }
            }
        }
    }
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

fn attach_tags(connection: &Connection, items: &mut [ImageRow]) -> AppResult<()> {
    if items.is_empty() {
        return Ok(());
    }
    let marks = placeholders(items.len());
    let sql = format!(
        "SELECT image_tags.image_id, tags.name
         FROM image_tags
         JOIN tags ON tags.id = image_tags.tag_id
         WHERE image_tags.image_id IN ({marks})
         ORDER BY tags.name COLLATE NOCASE"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(items.iter().map(|item| item.id)), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut by_image: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        let (image_id, name) = row?;
        by_image.entry(image_id).or_default().push(name);
    }
    for item in items {
        if let Some(tags) = by_image.remove(&item.id) {
            item.tags = tags;
        }
    }
    Ok(())
}

fn build_filter_counts(connection: &Connection, query: &LibraryQuery) -> AppResult<FilterCounts> {
    let total = {
        let mut filters = FilterSet::new();
        filters.push_view(query.view);
        filters.push_filters(&query.filters, None);
        count_rows(
            connection,
            &format!(
                "SELECT count(*) FROM images WHERE {}",
                filters.clauses.join(" AND ")
            ),
            &filters.params,
        )?
    };

    let folders = {
        let mut filters = FilterSet::new();
        filters.push_view(query.view);
        filters.push_filters(&query.filters, Some(Facet::Folders));
        grouped_i64(
            connection,
            &format!(
                "SELECT images.folder_id, count(*) FROM images WHERE {}
                 GROUP BY images.folder_id ORDER BY count(*) DESC, images.folder_id",
                filters.clauses.join(" AND ")
            ),
            &filters.params,
        )?
        .into_iter()
        .map(|(folder_id, count)| FolderCount { folder_id, count })
        .collect()
    };

    let extensions = {
        let mut filters = FilterSet::new();
        filters.push_view(query.view);
        filters.push_filters(&query.filters, Some(Facet::Extensions));
        let mut statement = connection.prepare(&format!(
            "SELECT images.file_ext, count(*) FROM images WHERE {}
             GROUP BY images.file_ext ORDER BY count(*) DESC, images.file_ext",
            filters.clauses.join(" AND ")
        ))?;
        let rows = statement.query_map(params_from_iter(filters.params.iter()), |row| {
            Ok(ValueCount {
                value: row.get(0)?,
                count: row.get::<_, i64>(1)?.max(0) as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let scores = {
        let mut filters = FilterSet::new();
        filters.push_view(query.view);
        filters.push_filters(&query.filters, Some(Facet::Scores));
        grouped_i64(
            connection,
            &format!(
                "SELECT images.score, count(*) FROM images WHERE {}
                 GROUP BY images.score ORDER BY images.score DESC",
                filters.clauses.join(" AND ")
            ),
            &filters.params,
        )?
        .into_iter()
        .map(|(score, count)| ValueCount {
            value: score.to_string(),
            count,
        })
        .collect()
    };

    let tags = {
        let mut filters = FilterSet::new();
        filters.push_view(query.view);
        filters.push_filters(&query.filters, Some(Facet::Tags));
        grouped_i64(
            connection,
            &format!(
                "SELECT image_tags.tag_id, count(*) FROM images
                 JOIN image_tags ON image_tags.image_id = images.id
                 WHERE {}
                 GROUP BY image_tags.tag_id ORDER BY count(*) DESC, image_tags.tag_id",
                filters.clauses.join(" AND ")
            ),
            &filters.params,
        )?
        .into_iter()
        .map(|(tag_id, count)| TagCount { tag_id, count })
        .collect()
    };

    Ok(FilterCounts {
        total,
        folders,
        extensions,
        scores,
        tags,
    })
}

fn count_rows(connection: &Connection, sql: &str, params: &[Value]) -> AppResult<u64> {
    let count: i64 =
        connection.query_row(sql, params_from_iter(params.iter()), |row| row.get(0))?;
    Ok(count.max(0) as u64)
}

fn grouped_i64(connection: &Connection, sql: &str, params: &[Value]) -> AppResult<Vec<(i64, u64)>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(params_from_iter(params.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as u64))
    })?;
    let values = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(values)
}

#[cfg(test)]
mod tests {
    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::{import::ensure_folder, service};

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        root: std::path::PathBuf,
        folder_people: i64,
        folder_scenery: i64,
        tag_sketch: i64,
        tag_reference: i64,
    }

    fn fixture() -> Fixture {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "查询测试", 2).unwrap();
        let (folder_people, folder_scenery, tag_sketch, tag_reference) = session
            .db
            .with_writer(|connection| {
                let people = ensure_folder(connection, "人物", 1)?;
                let scenery = ensure_folder(connection, "风景", 1)?;
                connection.execute("INSERT INTO tags(name) VALUES ('速写')", [])?;
                let sketch = connection.last_insert_rowid();
                connection.execute("INSERT INTO tags(name) VALUES ('参考')", [])?;
                let reference = connection.last_insert_rowid();

                insert_image(
                    connection,
                    "a",
                    "jpg",
                    3,
                    people,
                    1_000,
                    300,
                    640,
                    480,
                    None,
                    &[sketch, reference],
                );
                insert_image(
                    connection,
                    "b",
                    "png",
                    5,
                    people,
                    2_000,
                    200,
                    800,
                    600,
                    None,
                    &[sketch],
                );
                insert_image(
                    connection,
                    "c",
                    "jpg",
                    1,
                    scenery,
                    3_000,
                    100,
                    400,
                    400,
                    None,
                    &[],
                );
                insert_image(
                    connection,
                    "d",
                    "webp",
                    4,
                    scenery,
                    4_000,
                    400,
                    1200,
                    800,
                    None,
                    &[reference],
                );
                insert_image(
                    connection,
                    "e",
                    "png",
                    3,
                    people,
                    5_000,
                    500,
                    900,
                    900,
                    Some(1),
                    &[sketch, reference],
                );
                Ok((people, scenery, sketch, reference))
            })
            .unwrap();

        Fixture {
            _temp: temp,
            session,
            root,
            folder_people,
            folder_scenery,
            tag_sketch,
            tag_reference,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_image(
        connection: &Connection,
        seed: &str,
        extension: &str,
        score: i64,
        folder_id: i64,
        size: i64,
        imported_at: i64,
        width: i64,
        height: i64,
        deleted_at: Option<i64>,
        tags: &[i64],
    ) {
        let hash = format!("{seed:0>64}");
        connection
            .execute(
                "INSERT INTO images(
                    content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at,
                    modified_at, deleted_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?2, 0,
                    zeroblob(32), zeroblob(16), zeroblob(32), ?7, '', ?8, ?8, ?9)",
                params![
                    hash,
                    extension,
                    folder_id,
                    size,
                    width,
                    height,
                    score,
                    imported_at,
                    deleted_at
                ],
            )
            .unwrap();
        let image_id = connection.last_insert_rowid();
        for tag_id in tags {
            connection
                .execute(
                    "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)",
                    params![image_id, tag_id],
                )
                .unwrap();
        }
    }

    fn base_query() -> LibraryQuery {
        LibraryQuery {
            limit: Some(2),
            ..Default::default()
        }
    }

    fn ids(page: &ImagePage) -> Vec<i64> {
        page.items.iter().map(|item| item.id).collect()
    }

    fn all_ids(query: LibraryQuery, fixture: &Fixture) -> Vec<i64> {
        let mut cursor = None;
        let mut result = Vec::new();
        loop {
            let page = query_page(
                &fixture.session.db,
                &fixture.root,
                LibraryQuery {
                    cursor: cursor.clone(),
                    limit: Some(2),
                    ..query.clone()
                },
            )
            .unwrap();
            result.extend(ids(&page));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        result
    }

    #[test]
    fn pages_with_keyset_without_duplicates() {
        let fixture = fixture();
        let query = base_query();
        let first = query_page(&fixture.session.db, &fixture.root, query.clone()).unwrap();
        assert_eq!(ids(&first), vec![4, 1]);
        assert_eq!(
            first.next_cursor,
            Some(QueryCursor {
                sort_key: "300".into(),
                id: 1
            })
        );

        let collected = all_ids(query.clone(), &fixture);
        assert_eq!(collected, vec![4, 1, 2, 3]);

        let page = query_page(&fixture.session.db, &fixture.root, query).unwrap();
        let tag_names = &page.items[0].tags;
        assert_eq!(tag_names, &vec!["参考".to_owned()]);
        assert!(page.items[0].file_path.ends_with("0000d.webp"));
    }

    #[test]
    fn filters_by_folder_extension_score_and_tags() {
        let fixture = fixture();

        let folder = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![fixture.folder_people],
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&folder), vec![1, 2]);

        let extension = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    extensions: vec!["PNG".into()],
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&extension), vec![2]);

        let scores = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    scores: vec![1, 5],
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&scores), vec![2, 3]);

        let any = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    tag_ids: vec![fixture.tag_reference],
                    tag_mode: TagMode::Any,
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&any), vec![4, 1]);

        let all = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    tag_ids: vec![fixture.tag_sketch, fixture.tag_reference],
                    tag_mode: TagMode::All,
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&all), vec![1]);
    }

    #[test]
    fn sorts_by_score_and_size_and_views_trash() {
        let fixture = fixture();
        let score = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                sort: SortSpec {
                    key: SortKey::Score,
                    descending: true,
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&score), vec![2, 4]);

        let size_ascending = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                sort: SortSpec {
                    key: SortKey::Size,
                    descending: false,
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&size_ascending), vec![1, 2]);

        let trash = all_ids(
            LibraryQuery {
                view: LibraryView::Trash,
                ..base_query()
            },
            &fixture,
        );
        assert_eq!(trash, vec![5]);
    }

    #[test]
    fn reports_filter_counts_and_lists_folders_and_tags() {
        let fixture = fixture();
        let counts = filter_counts(
            &fixture.session.db,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![fixture.folder_people],
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(counts.total, 2);
        assert_eq!(
            counts
                .extensions
                .iter()
                .find(|item| item.value == "png")
                .map(|item| item.count),
            Some(1)
        );
        assert_eq!(
            counts
                .scores
                .iter()
                .find(|item| item.value == "3")
                .map(|item| item.count),
            Some(1)
        );
        // 文件夹分面不应用文件夹筛选，因此能看到其他文件夹。
        assert!(
            counts
                .folders
                .iter()
                .any(|item| item.folder_id == fixture.folder_scenery)
        );

        let folders = list_folders(&fixture.session.db).unwrap();
        assert_eq!(folders.len(), 3);
        assert_eq!(folders[1].path, "人物");
        assert_eq!(folders[1].image_count, 2);

        let tags = list_tags(&fixture.session.db, None).unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].name, "参考");
        assert_eq!(tags[0].image_count, 2);
    }

    #[test]
    fn keyset_handles_duplicate_sort_keys() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "游标测试", 1).unwrap();
        session
            .db
            .with_writer(|connection| {
                let folder = ensure_folder(connection, "", 1)?;
                for seed in ["1", "2", "3"] {
                    insert_image(
                        connection,
                        seed,
                        "jpg",
                        3,
                        folder,
                        100,
                        100,
                        10,
                        10,
                        None,
                        &[],
                    );
                }
                Ok(())
            })
            .unwrap();

        let mut cursor = None;
        let mut collected = Vec::new();
        loop {
            let page = query_page(
                &session.db,
                &root,
                LibraryQuery {
                    sort: SortSpec {
                        key: SortKey::Score,
                        descending: true,
                    },
                    cursor,
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
            collected.extend(ids(&page));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(collected, vec![3, 2, 1]);
    }

    #[test]
    fn returns_all_matching_ids() {
        let fixture = fixture();
        let ids = query_ids(
            &fixture.session.db,
            LibraryQuery {
                filters: LibraryFilters {
                    extensions: vec!["jpg".into()],
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn rejects_unsupported_query_parts() {
        let fixture = fixture();
        let result = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "人物".into(),
                ..base_query()
            },
        );
        assert!(result.is_err());

        let result = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::Collection,
                ..base_query()
            },
        );
        assert!(result.is_err());
    }
}

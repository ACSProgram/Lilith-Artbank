use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, params_from_iter, types::Value};

use crate::app::error::{AppError, AppResult};
use crate::library::{
    db::LibraryDb,
    paths::{escape_like, normalize_content_extension, subtree_like_pattern},
    placeholders, temp,
};

use super::page::parse_page_token;
use super::{
    DEFAULT_LIMIT, ImageRow, LibraryFilters, LibraryQuery, LibraryView, MAX_LIMIT, MAX_SORT_LEVELS,
    PresenceFilter, SearchScope, SortKey, SortSpec, TagMode, ValueCount,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Facet {
    Extensions,
    Scores,
    Tags,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchMode {
    /// 翻页：保留 OR 形式，能让 LIMIT 尽早停止，避免为 18 万条命中先建全集。
    Page,
    /// 计数/全选：一次性求出命中 id 的并集，避免每行重复相关子查询。
    Count,
}

/// 单个选中文件夹的子树条件：两个参数依次是路径本身与 `subtree_like_pattern`。
const FOLDER_SUBTREE_SQL: &str = "images.folder_id IN (
    SELECT scope_folders.id FROM folders scope_folders
    WHERE scope_folders.path = ? OR scope_folders.path LIKE ? ESCAPE '\\'
)";

pub(super) struct PreparedQuery {
    pub(super) join_sql: String,
    pub(super) join_params: Vec<Value>,
    pub(super) filters: FilterSet,
}

impl PreparedQuery {
    pub(super) fn new(
        query: &LibraryQuery,
        exclude: Option<Facet>,
        search_mode: SearchMode,
        folder_paths: &[String],
    ) -> Self {
        let mut prepared = Self {
            join_sql: String::new(),
            join_params: Vec::new(),
            filters: FilterSet::new(),
        };
        let library_scope = !query.search.is_empty() && query.search_scope == SearchScope::Library;
        // 全库搜索忽略当前视图，所以“临时视图 + 全库搜索”要按全库规则排除临时图片。
        let temp_view = !library_scope && query.view == LibraryView::Temp;
        if !library_scope {
            match query.view {
                LibraryView::All => prepared
                    .filters
                    .clauses
                    .push("images.deleted_at IS NULL".to_owned()),
                LibraryView::Trash => prepared
                    .filters
                    .clauses
                    .push("images.deleted_at IS NOT NULL".to_owned()),
                LibraryView::Collection => {
                    prepared.join_sql =
                        "JOIN collection_items ON collection_items.image_id = images.id \
                         AND collection_items.collection_id = ?"
                            .to_owned();
                    prepared
                        .join_params
                        .push(Value::Integer(query.collection_id.unwrap_or_default()));
                    prepared
                        .filters
                        .clauses
                        .push("images.deleted_at IS NULL".to_owned());
                }
                LibraryView::Temp => {
                    prepared
                        .filters
                        .clauses
                        .push("images.deleted_at IS NULL".to_owned());
                    prepared.filters.clauses.push(temp::ONLY_SQL.to_owned());
                }
            }
        }
        if !library_scope && query.view == LibraryView::Trash && query.filters.trash_loose {
            prepared.require_loose_trash();
        }
        if !temp_view {
            prepared.filters.clauses.push(temp::EXCLUDE_SQL.to_owned());
        }
        // 待释放行已经从回收站移除：回收站视图与全库搜索没有 `deleted_at IS NULL` 兜底，
        // 必须显式排除；其余视图由 `deleted_at IS NULL` 挡住（待释放行一定有 `deleted_at`）。
        // 不能无条件加这条：`purge_operation_id` 不在排序 id 列表的覆盖索引里，20 万行
        // 按文件夹排序的首页会从 39 ms 涨到 81 ms。
        if library_scope || query.view == LibraryView::Trash {
            prepared
                .filters
                .clauses
                .push("images.purge_operation_id IS NULL".to_owned());
        }
        prepared.filters.push_search(&query.search, search_mode);
        prepared
            .filters
            .push_filters(&query.filters, exclude, folder_paths);
        prepared
    }

    /// 回收站里的“直属图片”：原文件夹仍在图库（未整目录移入回收站）。
    fn require_loose_trash(&mut self) {
        self.filters.clauses.push(
            "images.folder_id NOT IN (
                SELECT trashed_folders.id FROM folders trashed_folders
                WHERE trashed_folders.trashed_at IS NOT NULL
            )"
            .to_owned(),
        );
    }

    /// 统计只算正常图片；全库搜索不带视图可见性条件时需要补上。
    pub(super) fn require_active_images(&mut self) {
        if !self
            .filters
            .clauses
            .iter()
            .any(|clause| clause == "images.deleted_at IS NULL")
        {
            self.filters
                .clauses
                .push("images.deleted_at IS NULL".to_owned());
        }
    }
}

pub(super) struct FilterSet {
    pub(super) clauses: Vec<String>,
    pub(super) params: Vec<Value>,
}

impl FilterSet {
    fn new() -> Self {
        Self {
            clauses: Vec::new(),
            params: Vec::new(),
        }
    }

    fn push_search(&mut self, search: &str, mode: SearchMode) {
        if search.is_empty() {
            return;
        }
        let like = format!("%{}%", escape_like(search));
        if mode == SearchMode::Page {
            let mut parts = Vec::new();
            if search.chars().count() >= 3 {
                parts.push(
                    "images.id IN (SELECT rowid FROM image_search WHERE image_search MATCH ?)"
                        .to_owned(),
                );
                self.params.push(Value::Text(fts_phrase(search)));
            } else {
                parts.push("images.description LIKE ? ESCAPE '\\'".to_owned());
                self.params.push(Value::Text(like.clone()));
            }
            parts.push(
                "EXISTS (SELECT 1 FROM image_tags search_links
                         JOIN tags search_tags ON search_tags.id = search_links.tag_id
                         WHERE search_links.image_id = images.id
                           AND search_tags.name LIKE ? ESCAPE '\\')"
                    .to_owned(),
            );
            self.params.push(Value::Text(like.clone()));
            parts.push(
                "EXISTS (SELECT 1 FROM folders search_folders
                         WHERE search_folders.id = images.folder_id
                           AND search_folders.path LIKE ? ESCAPE '\\')"
                    .to_owned(),
            );
            self.params.push(Value::Text(like));
            self.clauses.push(format!("({})", parts.join(" OR ")));
            return;
        }

        let description_query = if search.chars().count() >= 3 {
            self.params.push(Value::Text(fts_phrase(search)));
            "SELECT rowid FROM image_search WHERE image_search MATCH ?"
        } else {
            self.params.push(Value::Text(like.clone()));
            "SELECT search_images.id FROM images search_images
             WHERE search_images.description LIKE ? ESCAPE '\\'"
        };
        self.params.push(Value::Text(like.clone()));
        self.params.push(Value::Text(like));

        // 计数与全选一次性求命中 id 的并集，避免每个候选行都跑一次相关子查询。
        self.clauses.push(format!(
            "images.id IN (
                {description_query}
                UNION
                SELECT search_links.image_id FROM image_tags search_links
                JOIN tags search_tags ON search_tags.id = search_links.tag_id
                WHERE search_tags.name LIKE ? ESCAPE '\\'
                UNION
                SELECT search_images.id FROM images search_images
                JOIN folders search_folders ON search_folders.id = search_images.folder_id
                WHERE search_folders.path LIKE ? ESCAPE '\\'
            )"
        ));
    }

    fn push_filters(
        &mut self,
        filters: &LibraryFilters,
        exclude: Option<Facet>,
        folder_paths: &[String],
    ) {
        if !filters.folder_ids.is_empty() {
            if filters.folder_direct {
                let marks = placeholders(filters.folder_ids.len());
                self.clauses.push(format!("images.folder_id IN ({marks})"));
                self.params.extend(
                    filters
                        .folder_ids
                        .iter()
                        .map(|folder_id| Value::Integer(*folder_id)),
                );
            } else if folder_paths.iter().any(String::is_empty) {
                // 根目录的子树就是整库，等价于不限制文件夹。
            } else if folder_paths.is_empty() {
                self.clauses.push("0".to_owned());
            } else {
                // 每个选中路径一组参数：自身精确匹配 + 后代前缀匹配（路径边界避免
                // `人物` 命中 `人物速写`）。多选之间取并集。
                let parts = vec![FOLDER_SUBTREE_SQL.to_owned(); folder_paths.len()];
                for path in folder_paths {
                    self.params.push(Value::Text(path.clone()));
                    self.params.push(Value::Text(subtree_like_pattern(path)));
                }
                self.clauses.push(format!("({})", parts.join(" OR ")));
            }
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

        if exclude != Some(Facet::Tags) {
            self.push_tag_clauses(filters);
        }

        if let Some(from) = filters.imported_from {
            self.clauses.push("images.imported_at >= ?".to_owned());
            self.params.push(Value::Integer(from));
        }
        if let Some(to) = filters.imported_to {
            self.clauses.push("images.imported_at <= ?".to_owned());
            self.params.push(Value::Integer(to));
        }
        if let Some(min) = filters.size_min {
            self.clauses.push("images.file_size >= ?".to_owned());
            self.params.push(Value::Integer(min));
        }
        if let Some(max) = filters.size_max {
            self.clauses.push("images.file_size <= ?".to_owned());
            self.params.push(Value::Integer(max));
        }

        match filters.description {
            PresenceFilter::Any => {}
            PresenceFilter::Has => self.clauses.push("images.description <> ''".to_owned()),
            PresenceFilter::Missing => self.clauses.push("images.description = ''".to_owned()),
        }
        push_feature_presence(&mut self.clauses, "embedding", filters.embedding);
        push_feature_presence(&mut self.clauses, "pose", filters.pose);
    }

    /// 标签维度的三段条件：包含（any/all）、存在性、排除。
    ///
    /// 三段都算标签维度，做标签分面时一起跳过——选中一个标签后仍要能看到其它标签的计数。
    fn push_tag_clauses(&mut self, filters: &LibraryFilters) {
        if !filters.tag_ids.is_empty() {
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

        match filters.tag_presence {
            PresenceFilter::Any => {}
            PresenceFilter::Has => self.clauses.push(
                "EXISTS (SELECT 1 FROM image_tags WHERE image_tags.image_id = images.id)"
                    .to_owned(),
            ),
            PresenceFilter::Missing => self.clauses.push(
                "NOT EXISTS (SELECT 1 FROM image_tags WHERE image_tags.image_id = images.id)"
                    .to_owned(),
            ),
        }

        if !filters.exclude_tag_ids.is_empty() {
            let marks = placeholders(filters.exclude_tag_ids.len());
            self.clauses.push(format!(
                "NOT EXISTS (SELECT 1 FROM image_tags
                 WHERE image_tags.image_id = images.id
                   AND image_tags.tag_id IN ({marks}))"
            ));
            self.params.extend(
                filters
                    .exclude_tag_ids
                    .iter()
                    .map(|tag_id| Value::Integer(*tag_id)),
            );
        }
    }
}

pub(super) fn normalize(mut query: LibraryQuery) -> AppResult<(LibraryQuery, u32)> {
    query.search = query.search.trim().to_owned();
    if query.search.chars().count() > 200 {
        return Err(AppError::message("搜索文本不能超过 200 个字符"));
    }
    if query.search.is_empty() {
        query.search_scope = SearchScope::View;
    }
    if query.view == LibraryView::Trash {
        if query.filters.trash_loose {
            query.filters.folder_ids.clear();
            query.filters.folder_direct = false;
        }
    } else {
        query.filters.trash_loose = false;
    }

    if query.sort.is_empty() {
        query.sort.push(SortSpec::default());
    }
    if query.sort.len() > MAX_SORT_LEVELS {
        return Err(AppError::message(format!("排序最多 {MAX_SORT_LEVELS} 级")));
    }
    let mut sort_keys = HashSet::new();
    for spec in &query.sort {
        if !sort_keys.insert(spec.key) {
            return Err(AppError::message("排序指标不能重复"));
        }
    }
    let collection_position = query
        .sort
        .iter()
        .any(|spec| spec.key == SortKey::CollectionPosition);

    match query.view {
        LibraryView::Collection => {
            let collection_id = query
                .collection_id
                .ok_or_else(|| AppError::message("集合视图必须指定集合 id"))?;
            if collection_id <= 0 {
                return Err(AppError::message("集合 id 无效"));
            }
            if collection_position && query.search_scope == SearchScope::Library {
                return Err(AppError::message("全库搜索不能按集合内顺序排序"));
            }
        }
        _ => {
            if query.collection_id.is_some() {
                return Err(AppError::message("集合 id 只能用于集合视图"));
            }
            if collection_position {
                return Err(AppError::message("集合内顺序只能用于集合视图"));
            }
        }
    }

    if let (Some(from), Some(to)) = (query.filters.imported_from, query.filters.imported_to)
        && from > to
    {
        return Err(AppError::message("导入时间范围的起点晚于终点"));
    }
    if query.filters.size_min.is_some_and(|value| value < 0)
        || query.filters.size_max.is_some_and(|value| value < 0)
    {
        return Err(AppError::message("文件大小不能为负数"));
    }
    if let (Some(min), Some(max)) = (query.filters.size_min, query.filters.size_max)
        && min > max
    {
        return Err(AppError::message("文件大小范围的最小值大于最大值"));
    }

    query.filters.folder_ids = dedupe_ids(query.filters.folder_ids, 256, "文件夹")?;
    query.filters.tag_ids = dedupe_ids(query.filters.tag_ids, 64, "标签")?;
    query.filters.exclude_tag_ids = dedupe_ids(query.filters.exclude_tag_ids, 64, "标签")?;
    // “一个标签都没有”与“含有这些标签”互斥；同时给出时按错误处理，不静默丢弃一边。
    if query.filters.tag_presence == PresenceFilter::Missing && !query.filters.tag_ids.is_empty() {
        return Err(AppError::message(
            "筛选“没有任何标签”时不能再指定要包含的标签",
        ));
    }

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
        let values = cursor.sort_values()?;
        if values.len() != query.sort.len() {
            return Err(AppError::message("游标排序值与排序级数不一致"));
        }
        if cursor.id <= 0 {
            return Err(AppError::message("游标图片 id 无效"));
        }
    }
    if let Some(token) = &query.page_token {
        parse_page_token(token)?;
    }
    if query.anchor_id.is_some_and(|id| id <= 0) {
        return Err(AppError::message("锚点图片 id 无效"));
    }

    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    Ok((query, limit))
}

pub(super) fn dedupe_ids(values: Vec<i64>, max: usize, label: &str) -> AppResult<Vec<i64>> {
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

pub(super) fn sort_expression(key: SortKey) -> &'static str {
    match key {
        SortKey::ImportedAt => "images.imported_at",
        SortKey::ModifiedAt => "images.modified_at",
        SortKey::Score => "images.score",
        SortKey::Size => "images.file_size",
        SortKey::Pixels => "(images.width * images.height)",
        SortKey::Folder => "folders.path COLLATE NOCASE",
        SortKey::CollectionPosition => "collection_items.position",
    }
}

fn direction(descending: bool) -> &'static str {
    if descending { "DESC" } else { "ASC" }
}

pub(super) fn order_by(sort: &[SortSpec]) -> String {
    let mut parts: Vec<String> = sort
        .iter()
        .map(|spec| {
            format!(
                "{} {}",
                sort_expression(spec.key),
                direction(spec.descending)
            )
        })
        .collect();
    // 末级方向也决定 id 次序，保证跨页稳定。
    let last = sort.last().copied().unwrap_or_default();
    parts.push(format!("images.id {}", direction(last.descending)));
    parts.join(", ")
}

/// 单级且不是文件夹（文本列）时走键集游标；多级或文件夹排序走有序 id 列表。
pub(super) fn uses_id_cursor(sort: &[SortSpec]) -> bool {
    sort.len() > 1 || sort.first().is_some_and(|spec| spec.key == SortKey::Folder)
}

pub(super) fn keyset_condition(
    sort: &[SortSpec],
    values: &[i64],
    cursor_id: i64,
) -> (String, Vec<Value>) {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    for level in 0..sort.len() {
        let mut terms = Vec::new();
        for index in 0..level {
            terms.push(format!("{} = ?", sort_expression(sort[index].key)));
            params.push(Value::Integer(values[index]));
        }
        terms.push(format!(
            "{} {} ?",
            sort_expression(sort[level].key),
            if sort[level].descending { "<" } else { ">" }
        ));
        params.push(Value::Integer(values[level]));
        clauses.push(format!("({})", terms.join(" AND ")));
    }
    let mut terms = Vec::new();
    for (index, spec) in sort.iter().enumerate() {
        terms.push(format!("{} = ?", sort_expression(spec.key)));
        params.push(Value::Integer(values[index]));
    }
    terms.push(format!(
        "images.id {} ?",
        if sort.last().is_some_and(|spec| spec.descending) {
            "<"
        } else {
            ">"
        }
    ));
    params.push(Value::Integer(cursor_id));
    clauses.push(format!("({})", terms.join(" AND ")));
    (format!("({})", clauses.join(" OR ")), params)
}

fn push_feature_presence(clauses: &mut Vec<String>, kind: &str, presence: PresenceFilter) {
    match presence {
        PresenceFilter::Any => {}
        PresenceFilter::Has => clauses.push(format!(
            "EXISTS (SELECT 1 FROM feature_blobs WHERE feature_blobs.image_id = images.id \
             AND feature_blobs.kind = '{kind}')"
        )),
        PresenceFilter::Missing => clauses.push(format!(
            "NOT EXISTS (SELECT 1 FROM feature_blobs WHERE feature_blobs.image_id = images.id \
             AND feature_blobs.kind = '{kind}')"
        )),
    }
}

fn fts_phrase(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub(super) fn attach_tags(connection: &Connection, items: &mut [ImageRow]) -> AppResult<()> {
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

pub(super) fn image_row_from_row(
    row: &rusqlite::Row<'_>,
    root: &Path,
) -> rusqlite::Result<ImageRow> {
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
    Ok(ImageRow {
        id: row.get(0)?,
        content_hash,
        file_ext,
        folder_id: row.get(3)?,
        folder_path,
        folder_trashed: row.get::<_, i64>(17)? != 0,
        file_path: file_path.to_string_lossy().into_owned(),
        file_size: row.get(5)?,
        width: row.get(6)?,
        height: row.get(7)?,
        format: row.get(8)?,
        has_alpha: row.get::<_, i64>(9)? != 0,
        score: row.get(10)?,
        imported_at: row.get(11)?,
        modified_at: row.get(12)?,
        deleted_at: row.get(13)?,
        thumb_version: row.get(14)?,
        thumb_width: row.get(15)?,
        thumb_height: row.get(16)?,
        tags: Vec::new(),
    })
}

/// 按给定 id 顺序取完整行；用于语义搜索结果的分页回填。
pub fn rows_by_ids(database: &LibraryDb, root: &Path, ids: &[i64]) -> AppResult<Vec<ImageRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = crate::library::placeholders(ids.len());
    let sql = format!(
        "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                folders.path, images.file_size, images.width, images.height,
                images.format, images.has_alpha, images.score, images.imported_at,
                images.modified_at, images.deleted_at, images.thumb_version,
                images.thumb_width, images.thumb_height, folders.trashed_at IS NOT NULL
         FROM images
         JOIN folders ON folders.id = images.folder_id
         WHERE images.id IN ({placeholders}) AND images.purge_operation_id IS NULL"
    );
    let values: Vec<Value> = ids.iter().map(|id| Value::Integer(*id)).collect();
    database.with_reader(|connection| {
        let mut statement = connection.prepare(&sql)?;
        let rows = statement
            .query_map(params_from_iter(values.iter()), |row| {
                image_row_from_row(row, root)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut by_id: HashMap<i64, ImageRow> = rows.into_iter().map(|row| (row.id, row)).collect();
        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = by_id.remove(id) {
                items.push(row);
            }
        }
        attach_tags(connection, &mut items)?;
        Ok(items)
    })
}

pub(super) fn rows_for_ids(
    connection: &Connection,
    root: &Path,
    ids: &[i64],
) -> AppResult<Vec<ImageRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = crate::library::placeholders(ids.len());
    let sql = format!(
        "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                folders.path, images.file_size, images.width, images.height,
                images.format, images.has_alpha, images.score, images.imported_at,
                images.modified_at, images.deleted_at, images.thumb_version,
                images.thumb_width, images.thumb_height, folders.trashed_at IS NOT NULL
         FROM images
         JOIN folders ON folders.id = images.folder_id
         WHERE images.id IN ({placeholders}) AND images.purge_operation_id IS NULL"
    );
    let values: Vec<Value> = ids.iter().map(|id| Value::Integer(*id)).collect();
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(params_from_iter(values.iter()), |row| {
            image_row_from_row(row, root)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut by_id: HashMap<i64, ImageRow> = rows.into_iter().map(|row| (row.id, row)).collect();
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(row) = by_id.remove(id) {
            items.push(row);
        }
    }
    Ok(items)
}

pub(super) fn filters_only(
    filters: &LibraryFilters,
    exclude: Option<Facet>,
    folder_paths: &[String],
) -> PreparedQuery {
    let mut prepared = PreparedQuery {
        join_sql: String::new(),
        join_params: Vec::new(),
        filters: FilterSet::new(),
    };
    prepared
        .filters
        .push_filters(filters, exclude, folder_paths);
    prepared
}

/// 把选中的文件夹 id 解析成路径；已不存在或非法的 id 直接丢弃。
pub(super) fn folder_scope_paths(connection: &Connection, ids: &[i64]) -> AppResult<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let marks = placeholders(ids.len());
    let mut statement =
        connection.prepare(&format!("SELECT path FROM folders WHERE id IN ({marks})"))?;
    let paths = statement
        .query_map(params_from_iter(ids.iter().copied()), |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(paths)
}

pub(super) fn push_count_branch(
    branches: &mut Vec<String>,
    params: &mut Vec<Value>,
    select: &str,
    suffix: &str,
    prepared: &PreparedQuery,
) {
    params.extend(prepared.filters.params.iter().cloned());
    let filter = if prepared.filters.clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", prepared.filters.clauses.join(" AND "))
    };
    branches.push(format!("{select} {filter} {suffix}"));
}

pub(super) fn parse_count_key(value: &str) -> AppResult<i64> {
    value
        .parse()
        .map_err(|_| AppError::message("筛选计数键无效"))
}

pub(super) fn count_sql(prepared: &PreparedQuery) -> (String, Vec<Value>) {
    let mut params = prepared.join_params.clone();
    params.extend(prepared.filters.params.iter().cloned());
    let sql = format!(
        "SELECT count(*) FROM images {} WHERE {}",
        prepared.join_sql,
        prepared.filters.clauses.join(" AND ")
    );
    (sql, params)
}

fn index_hint(index: Option<&str>) -> String {
    index
        .map(|name| format!(" INDEXED BY {name}"))
        .unwrap_or_default()
}

pub(super) fn sum_sql(prepared: &PreparedQuery, index: Option<&str>) -> (String, Vec<Value>) {
    let mut params = prepared.join_params.clone();
    params.extend(prepared.filters.params.iter().cloned());
    let hint = index_hint(index);
    let sql = format!(
        "SELECT coalesce(sum(images.file_size), 0) FROM images{hint} {} WHERE {}",
        prepared.join_sql,
        prepared.filters.clauses.join(" AND ")
    );
    (sql, params)
}

pub(super) fn grouped_sql(
    prepared: &PreparedQuery,
    index: Option<&str>,
    columns: &str,
    suffix: &str,
) -> (String, Vec<Value>) {
    let mut params = prepared.join_params.clone();
    params.extend(prepared.filters.params.iter().cloned());
    let hint = index_hint(index);
    let sql = format!(
        "SELECT {columns} FROM images{hint} {} WHERE {} {suffix}",
        prepared.join_sql,
        prepared.filters.clauses.join(" AND ")
    );
    (sql, params)
}

pub(super) fn tags_grouped_sql(
    prepared: &PreparedQuery,
    index: Option<&str>,
) -> (String, Vec<Value>) {
    let mut params = prepared.join_params.clone();
    params.extend(prepared.filters.params.iter().cloned());
    let hint = index_hint(index);
    let sql = format!(
        "SELECT image_tags.tag_id, count(*) FROM images{hint}
         JOIN image_tags ON image_tags.image_id = images.id
         {} WHERE {}
         GROUP BY image_tags.tag_id ORDER BY count(*) DESC, image_tags.tag_id",
        prepared.join_sql,
        prepared.filters.clauses.join(" AND ")
    );
    (sql, params)
}

pub(super) fn count_rows(connection: &Connection, sql: &str, params: &[Value]) -> AppResult<u64> {
    let count: i64 =
        connection.query_row(sql, params_from_iter(params.iter()), |row| row.get(0))?;
    Ok(count.max(0) as u64)
}

pub(super) fn sum_rows(connection: &Connection, sql: &str, params: &[Value]) -> AppResult<u64> {
    let sum: i64 = connection.query_row(sql, params_from_iter(params.iter()), |row| row.get(0))?;
    Ok(sum.max(0) as u64)
}

pub(super) fn grouped_values(
    connection: &Connection,
    sql: &str,
    params: &[Value],
) -> AppResult<Vec<ValueCount>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(params_from_iter(params.iter()), |row| {
        Ok(ValueCount {
            value: row.get(0)?,
            count: row.get::<_, i64>(1)?.max(0) as u64,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn grouped_i64(
    connection: &Connection,
    sql: &str,
    params: &[Value],
) -> AppResult<Vec<(i64, u64)>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(params_from_iter(params.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as u64))
    })?;
    let values = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(values)
}

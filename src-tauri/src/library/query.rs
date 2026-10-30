use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
};

use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value};
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        folder_tree::FolderTree,
        paths::{escape_like, normalize_content_extension, subtree_like_pattern},
        placeholders, temp,
    },
};

pub const DEFAULT_LIMIT: u32 = 150;
pub const MAX_LIMIT: u32 = 300;
pub const MAX_TAGS: u32 = 500;
/// 标签管理器一次要看全（含 0 张标签）才数得准「要清理多少个」。
///
/// 只有 `include_empty = true` 这条管理路径放宽；滚动、搜索、分面这些热路径仍是
/// [`MAX_TAGS`]。0 张标签按定义没有关系行，逐行子查询都是空集，很便宜。
pub const MAX_TAGS_WITH_EMPTY: u32 = 5_000;
const MAX_IDS: usize = 1_000_000;
/// 排序级数上限：等于可用指标数量，每个指标最多用一次。设置文件的校验沿用这个值。
pub const MAX_SORT_LEVELS: usize = 7;
/// id 游标只保留最近两份有序 id 列表，够“当前页 + 上一份”回退。
const MAX_CACHED_LISTS: usize = 2;

/// 多级排序或没有可走索引的指标时，一次性算出有序 id 列表并按 offset 切页。
/// 缓存放在进程内，token 只在前端来回传递；库关闭或数据变化时清空。
struct CachedIdList {
    token: String,
    fingerprint: String,
    ids: Arc<Vec<i64>>,
}

static ID_PAGE_CACHE: Mutex<Vec<CachedIdList>> = Mutex::new(Vec::new());

/// 让已缓存的有序 id 列表整体失效：数据变更后前端会重新取第一页，旧 token 即便被用也会报“已过期”。
///
/// 任何改变图片集合、排序字段或筛选字段的写入都要调用（`AppState::mutate` 与导入收尾已覆盖）。
pub fn clear_page_cache() {
    if let Ok(mut cache) = ID_PAGE_CACHE.lock() {
        cache.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LibraryView {
    #[default]
    All,
    Trash,
    Collection,
    /// 临时文件夹；只出现在视图栏的固定入口，不参与默认视图与搜索。
    Temp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SortKey {
    #[default]
    ImportedAt,
    ModifiedAt,
    Score,
    Size,
    Pixels,
    Folder,
    CollectionPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SearchScope {
    #[default]
    View,
    Library,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PresenceFilter {
    #[default]
    Any,
    Has,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TagMode {
    #[default]
    Any,
    All,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryFilters {
    pub folder_ids: Vec<i64>,
    /// true 时只匹配直接位于选中文件夹的图片，不展开子文件夹。
    pub folder_direct: bool,
    /// 回收站视图的“直属图片”：只匹配原文件夹仍在图库中的回收站图片。
    pub trash_loose: bool,
    pub extensions: Vec<String>,
    pub scores: Vec<i64>,
    pub tag_ids: Vec<i64>,
    pub tag_mode: TagMode,
    /// 标签的存在性：any 不限、has 至少一个标签、missing 一个标签都没有。
    pub tag_presence: PresenceFilter,
    /// 排除拥有其中任一标签的图片；与 `tag_ids` 独立叠加。
    pub exclude_tag_ids: Vec<i64>,
    pub imported_from: Option<i64>,
    pub imported_to: Option<i64>,
    pub size_min: Option<i64>,
    pub size_max: Option<i64>,
    pub description: PresenceFilter,
    pub embedding: PresenceFilter,
    pub pose: PresenceFilter,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryQuery {
    pub view: LibraryView,
    pub collection_id: Option<i64>,
    pub search: String,
    pub search_scope: SearchScope,
    pub filters: LibraryFilters,
    /// 第一级为主排序；级数不超过指标数量。
    pub sort: Vec<SortSpec>,
    /// 单级排序的键集游标；多级或文件夹排序走 `page_token`。
    pub cursor: Option<QueryCursor>,
    /// id 游标通道：`<列表 token>:<offset>`。
    pub page_token: Option<String>,
    /// 跳转锚点：从这张图所在的位置开始取一页（包含锚点本身）。
    pub anchor_id: Option<i64>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueryCursor {
    pub sort_values: Vec<String>,
    pub id: i64,
}

impl QueryCursor {
    // 游标排序值是排序列的原始整数；用字符串跨 IPC 传递，避免前端数值精度问题。
    fn sort_values(&self) -> AppResult<Vec<i64>> {
        self.sort_values
            .iter()
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| AppError::message("游标排序值无效"))
            })
            .collect()
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
    /// 所在文件夹已整目录移入回收站；这类图片不提供单张恢复/永久删除。
    pub folder_trashed: bool,
    pub file_path: String,
    pub file_size: i64,
    pub width: i64,
    pub height: i64,
    pub format: String,
    pub has_alpha: bool,
    pub score: i64,
    pub imported_at: i64,
    pub modified_at: i64,
    pub deleted_at: Option<i64>,
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
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    pub extensions: Vec<ValueCount>,
    pub scores: Vec<ValueCount>,
    pub tags: Vec<TagCount>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsTag {
    pub id: i64,
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderStats {
    pub image_count: u64,
    pub total_bytes: u64,
    /// 选中范围（子树并集）内的子文件夹数，不含选中的文件夹自身。
    pub subfolder_count: u64,
    pub extensions: Vec<ValueCount>,
    pub scores: Vec<ValueCount>,
    pub tags: Vec<StatsTag>,
}

const FOLDER_STATS_TAG_LIMIT: usize = 8;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderInfo {
    pub id: i64,
    pub path: String,
    /// 同一父级下的手动排序位。
    pub sort_index: i64,
    /// 直接位于该文件夹的未删除图片数。
    pub direct_image_count: u64,
    /// 包含全部后代的未删除图片数。
    pub total_image_count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderChange {
    /// 受影响文件夹的最新状态；前端按 id 就地合并。
    pub folders: Vec<FolderInfo>,
    /// 已经从正常树移除的文件夹 id。
    pub removed_ids: Vec<i64>,
    /// 这次操作涉及的图片数；其它文件夹写命令为 0。
    pub affected_images: u64,
}

/// 回收站文件夹树：`folders` 只含已移入回收站的文件夹，计数都是回收站图片数。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashFolders {
    pub folders: Vec<FolderInfo>,
    /// 原文件夹仍在图库中的回收站图片数（回收站根行的“直属图片”）。
    pub direct_image_count: u64,
    /// 回收站里的全部图片数，含已删除文件夹下的图片。
    pub total_image_count: u64,
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
        let folder_paths = folder_scope_paths(connection, &query.filters.folder_ids)?;
        if uses_id_cursor(&query.sort) || query.page_token.is_some() || query.anchor_id.is_some() {
            return page_from_id_list(connection, root, &query, &folder_paths, limit);
        }
        page_from_keyset(connection, root, &query, &folder_paths, limit)
    })
}

fn page_from_keyset(
    connection: &Connection,
    root: &Path,
    query: &LibraryQuery,
    folder_paths: &[String],
    limit: u32,
) -> AppResult<ImagePage> {
    let mut prepared = PreparedQuery::new(query, None, SearchMode::Page, folder_paths);
    if let Some(cursor) = &query.cursor {
        let values = cursor.sort_values()?;
        if values.len() != query.sort.len() {
            return Err(AppError::message("游标排序值与排序级数不一致"));
        }
        let (clause, params) = keyset_condition(&query.sort, &values, cursor.id);
        prepared.filters.clauses.push(clause);
        prepared.filters.params.extend(params);
    }

    let mut params = prepared.join_params.clone();
    params.extend(prepared.filters.params.iter().cloned());
    params.push(Value::Integer(i64::from(limit)));

    // 末尾依次多选每一级排序列：游标键直接从行里读。
    let sort_selects = query
        .sort
        .iter()
        .map(|spec| sort_expression(spec.key))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                folders.path, images.file_size, images.width, images.height,
                images.format, images.has_alpha, images.score, images.imported_at,
                images.modified_at, images.deleted_at, images.thumb_version,
                images.thumb_width, images.thumb_height,
                folders.trashed_at IS NOT NULL, {sort_selects}
         FROM images
         JOIN folders ON folders.id = images.folder_id
         {} WHERE {}
         ORDER BY {}
         LIMIT ?",
        prepared.join_sql,
        prepared.filters.clauses.join(" AND "),
        order_by(&query.sort)
    );

    let mut statement = connection.prepare(&sql)?;
    let sort_count = query.sort.len();
    let rows = statement.query_map(params_from_iter(params.iter()), |row| {
        let item = image_row_from_row(row, root)?;
        let mut sort_values = Vec::with_capacity(sort_count);
        for index in 0..sort_count {
            sort_values.push(row.get::<_, i64>(18 + index)?);
        }
        Ok((item, sort_values))
    })?;

    let rows_with_keys = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let next_cursor = if rows_with_keys.len() == limit as usize {
        rows_with_keys
            .last()
            .map(|(item, sort_values)| QueryCursor {
                sort_values: sort_values.iter().map(i64::to_string).collect(),
                id: item.id,
            })
    } else {
        None
    };

    let mut items: Vec<ImageRow> = rows_with_keys.into_iter().map(|(item, _)| item).collect();
    attach_tags(connection, &mut items)?;
    Ok(ImagePage {
        items,
        next_cursor,
        next_page_token: None,
    })
}

/// 多级排序或文件夹排序：一次性算出有序 id 列表并按 offset 切页。
fn page_from_id_list(
    connection: &Connection,
    root: &Path,
    query: &LibraryQuery,
    folder_paths: &[String],
    limit: u32,
) -> AppResult<ImagePage> {
    let fingerprint = list_fingerprint(query);
    let (token, ids) = match &query.page_token {
        Some(page_token) => {
            let (list_token, _) = parse_page_token(page_token)?;
            let cached = ID_PAGE_CACHE.lock().ok().and_then(|cache| {
                cache
                    .iter()
                    .find(|item| item.token == list_token)
                    .map(|item| (item.fingerprint.clone(), item.ids.clone()))
            });
            match cached {
                Some((cached_fingerprint, ids)) if cached_fingerprint == fingerprint => {
                    (list_token.to_owned(), ids)
                }
                _ => return Err(AppError::message("排序结果已过期，请重新载入")),
            }
        }
        None => {
            let ids = Arc::new(build_sorted_ids(connection, query, folder_paths)?);
            let token = uuid::Uuid::new_v4().to_string();
            cache_id_list(&token, &fingerprint, ids.clone());
            (token, ids)
        }
    };

    let offset = if let Some(page_token) = &query.page_token {
        parse_page_token(page_token)?.1
    } else if let Some(anchor) = query.anchor_id {
        match ids.iter().position(|id| *id == anchor) {
            Some(position) => position,
            // 锚点不在结果里：返回空页，调用方按“没找到”处理。
            None => {
                return Ok(ImagePage {
                    items: Vec::new(),
                    next_cursor: None,
                    next_page_token: None,
                });
            }
        }
    } else {
        0
    };

    let end = offset.saturating_add(limit as usize).min(ids.len());
    let slice = ids.get(offset..end).unwrap_or(&[]);
    let mut items = rows_for_ids(connection, root, slice)?;
    attach_tags(connection, &mut items)?;
    let next_page_token = (end < ids.len()).then(|| format!("{token}:{end}"));
    Ok(ImagePage {
        items,
        next_cursor: None,
        next_page_token,
    })
}

fn build_sorted_ids(
    connection: &Connection,
    query: &LibraryQuery,
    folder_paths: &[String],
) -> AppResult<Vec<i64>> {
    let prepared = PreparedQuery::new(query, None, SearchMode::Count, folder_paths);
    let mut params = prepared.join_params.clone();
    params.extend(prepared.filters.params.iter().cloned());
    params.push(Value::Integer(MAX_IDS as i64 + 1));
    let sql = format!(
        "SELECT images.id FROM images
         JOIN folders ON folders.id = images.folder_id
         {} WHERE {}
         ORDER BY {}
         LIMIT ?",
        prepared.join_sql,
        prepared.filters.clauses.join(" AND "),
        order_by(&query.sort)
    );
    let mut statement = connection.prepare(&sql)?;
    let ids = statement
        .query_map(params_from_iter(params.iter()), |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if ids.len() > MAX_IDS {
        return Err(AppError::message("当前筛选结果超过 100 万张，无法排序"));
    }
    Ok(ids)
}

fn parse_page_token(token: &str) -> AppResult<(&str, usize)> {
    let (list_token, offset) = token
        .rsplit_once(':')
        .ok_or_else(|| AppError::message("分页 token 无效"))?;
    let offset = offset
        .parse()
        .map_err(|_| AppError::message("分页 token 无效"))?;
    Ok((list_token, offset))
}

fn list_fingerprint(query: &LibraryQuery) -> String {
    let mut normalized = query.clone();
    normalized.cursor = None;
    normalized.page_token = None;
    normalized.anchor_id = None;
    normalized.limit = None;
    format!("{normalized:?}")
}

fn cache_id_list(token: &str, fingerprint: &str, ids: Arc<Vec<i64>>) {
    let Ok(mut cache) = ID_PAGE_CACHE.lock() else {
        return;
    };
    cache.insert(
        0,
        CachedIdList {
            token: token.to_owned(),
            fingerprint: fingerprint.to_owned(),
            ids,
        },
    );
    cache.truncate(MAX_CACHED_LISTS);
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

fn rows_for_ids(connection: &Connection, root: &Path, ids: &[i64]) -> AppResult<Vec<ImageRow>> {
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

fn image_row_from_row(row: &rusqlite::Row<'_>, root: &Path) -> rusqlite::Result<ImageRow> {
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

pub fn query_ids(database: &LibraryDb, query: LibraryQuery) -> AppResult<Vec<i64>> {
    // 全选只取 id，不翻页；游标与 limit 在这里没有意义。
    let (query, _) = normalize(query)?;
    database.with_reader(|connection| {
        let folder_paths = folder_scope_paths(connection, &query.filters.folder_ids)?;
        let prepared = PreparedQuery::new(&query, None, SearchMode::Count, &folder_paths);
        let mut params = prepared.join_params.clone();
        params.extend(prepared.filters.params.iter().cloned());
        params.push(Value::Integer(MAX_IDS as i64 + 1));

        let sql = format!(
            "SELECT images.id FROM images
             JOIN folders ON folders.id = images.folder_id
             {} WHERE {}
             ORDER BY {}
             LIMIT ?",
            prepared.join_sql,
            prepared.filters.clauses.join(" AND "),
            order_by(&query.sort),
        );

        let mut statement = connection.prepare(&sql)?;
        let ids = statement
            .query_map(params_from_iter(params.iter()), |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        if ids.len() > MAX_IDS {
            return Err(AppError::message("当前筛选结果超过 100 万张，无法一次全选"));
        }
        Ok(ids)
    })
}

/// 相似分组合并的默认保留规则：评分高优先，其次导入时间晚优先。
pub const DEFAULT_KEEP_SORT: [SortSpec; 2] = [
    SortSpec {
        key: SortKey::Score,
        descending: true,
    },
    SortSpec {
        key: SortKey::ImportedAt,
        descending: true,
    },
];

/// 合并时可用于挑保留项的指标；“集合内顺序”跨集合没有意义，不在其中。
pub const KEEP_SORT_KEYS: [SortKey; 6] = [
    SortKey::Score,
    SortKey::ImportedAt,
    SortKey::ModifiedAt,
    SortKey::Pixels,
    SortKey::Size,
    SortKey::Folder,
];

/// 一次最多挑多少组；与相似分组的结果上限一致。
const MAX_KEEP_GROUPS: usize = crate::library::similarity::MAX_GROUPS;
/// 单个组的成员上限；与 pHash 分组的单组上限一致。
const MAX_KEEP_GROUP_SIZE: usize = crate::library::similarity::MAX_GROUP_SIZE;

/// 合并相似组时每组保留哪一张：按给定排序取每组的第一张，规则与浏览排序一致。
///
/// 返回与 `groups` 等长的数组，元素是保留的图片 id；组内没有可见图片时是 `None`。
/// 只读，不改任何状态；调用方拿它算出要移入回收站的其余 id。
pub fn group_keeps(
    database: &LibraryDb,
    groups: &[Vec<i64>],
    sort: &[SortSpec],
) -> AppResult<Vec<Option<i64>>> {
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    if groups.len() > MAX_KEEP_GROUPS {
        return Err(AppError::message(format!(
            "一次最多处理 {MAX_KEEP_GROUPS} 个组"
        )));
    }
    let sort = normalize_keep_sort(sort)?;
    let order = order_by(&sort);
    database.with_reader(|connection| {
        let mut keeps = Vec::with_capacity(groups.len());
        for group in groups {
            if group.len() > MAX_KEEP_GROUP_SIZE {
                return Err(AppError::message(format!(
                    "单个组超过 {MAX_KEEP_GROUP_SIZE} 张，无法自动挑选保留项"
                )));
            }
            let ids = dedupe_ids(group.clone(), MAX_KEEP_GROUP_SIZE, "分组图片")?;
            if ids.is_empty() {
                keeps.push(None);
                continue;
            }
            let marks = placeholders(ids.len());
            let sql = format!(
                "SELECT images.id FROM images
                 LEFT JOIN folders ON folders.id = images.folder_id
                 WHERE images.id IN ({marks}) AND images.deleted_at IS NULL
                 ORDER BY {order}
                 LIMIT 1"
            );
            let mut statement = connection.prepare(&sql)?;
            let keep = statement
                .query_row(params_from_iter(ids.iter()), |row| row.get::<_, i64>(0))
                .optional()?;
            keeps.push(keep);
        }
        Ok(keeps)
    })
}

/// 保留规则只允许白名单指标，去重后不超过排序级数上限；空规则回落到默认值。
fn normalize_keep_sort(sort: &[SortSpec]) -> AppResult<Vec<SortSpec>> {
    let mut levels: Vec<SortSpec> = Vec::with_capacity(sort.len());
    for spec in sort {
        if !KEEP_SORT_KEYS.contains(&spec.key) {
            return Err(AppError::message("这个指标不能用来决定保留哪张"));
        }
        if levels.iter().any(|level| level.key == spec.key) {
            continue;
        }
        levels.push(*spec);
    }
    if levels.is_empty() {
        levels.extend_from_slice(&DEFAULT_KEEP_SORT);
    }
    if levels.len() > MAX_SORT_LEVELS {
        return Err(AppError::message(format!("排序最多 {MAX_SORT_LEVELS} 级")));
    }
    Ok(levels)
}

pub fn filter_counts(database: &LibraryDb, query: LibraryQuery) -> AppResult<FilterCounts> {
    let (query, _) = normalize(query)?;
    database.with_reader(|connection| {
        let folder_paths = folder_scope_paths(connection, &query.filters.folder_ids)?;
        build_filter_counts(connection, &query, &folder_paths)
    })
}

/// 只算命中总数，不取行；用于搜索结果头部的 `total`。
pub fn query_total(database: &LibraryDb, query: LibraryQuery) -> AppResult<u64> {
    let (query, _) = normalize(query)?;
    database.with_reader(|connection| {
        let folder_paths = folder_scope_paths(connection, &query.filters.folder_ids)?;
        let prepared = PreparedQuery::new(&query, None, SearchMode::Count, &folder_paths);
        let (sql, params) = count_sql(&prepared);
        count_rows(connection, &sql, &params)
    })
}

/// 批量任务开始前的“是否仍待处理”条件。
///
/// 预览与任务启动共用这一份 SQL，避免“开始前的数字”和任务 `total` 各算一套。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingFilter {
    /// 重做类任务：只要行未删除就处理。
    All,
    Thumbnail,
    Embedding {
        model: String,
        dim: u32,
    },
    Pose,
    Description {
        overwrite: bool,
    },
}

impl PendingFilter {
    /// 返回条件并把它用到的参数按 SQL 文本顺序追加上去。
    ///
    /// 调用方必须把这里的参数放在 join 与筛选参数之前：条件出现在 SELECT 列表，
    /// 在 SQL 文本里早于 `WHERE`。
    fn clause(&self, params: &mut Vec<Value>) -> String {
        let active = "images.deleted_at IS NULL";
        match self {
            Self::All => active.to_owned(),
            Self::Thumbnail => {
                format!("{active} AND (images.thumb_segment = 0 OR images.thumb_length <= 0)")
            }
            Self::Embedding { model, dim } => {
                params.push(Value::Text(model.clone()));
                params.push(Value::Integer(i64::from(*dim)));
                format!(
                    "{active} AND NOT EXISTS (
                         SELECT 1 FROM feature_blobs pending_features
                         WHERE pending_features.image_id = images.id
                           AND pending_features.kind = 'embedding'
                           AND pending_features.model = ?
                           AND pending_features.dim = ?)"
                )
            }
            Self::Pose => format!(
                "{active} AND NOT EXISTS (
                     SELECT 1 FROM feature_blobs pending_features
                     WHERE pending_features.image_id = images.id
                       AND pending_features.kind = 'pose')"
            ),
            Self::Description { overwrite: false } => {
                format!("{active} AND length(trim(images.description)) = 0")
            }
            Self::Description { overwrite: true } => {
                format!("{active} AND images.description_source <> 'user'")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingStats {
    /// 范围在浏览口径下的行数，可能包含回收站里的图片。
    pub total: u64,
    /// 范围里未删除且确实待处理的行数。
    pub will_process: u64,
}

/// 非选择范围的待处理统计：一条聚合扫过范围内的所有行。
pub fn pending_stats(
    database: &LibraryDb,
    query: LibraryQuery,
    filter: &PendingFilter,
) -> AppResult<PendingStats> {
    let (query, _) = normalize(query)?;
    database.with_reader(|connection| {
        let folder_paths = folder_scope_paths(connection, &query.filters.folder_ids)?;
        let prepared = PreparedQuery::new(&query, None, SearchMode::Count, &folder_paths);
        let mut params = Vec::new();
        let pending = filter.clause(&mut params);
        params.extend(prepared.join_params.iter().cloned());
        params.extend(prepared.filters.params.iter().cloned());
        let filters = if prepared.filters.clauses.is_empty() {
            "1".to_owned()
        } else {
            prepared.filters.clauses.join(" AND ")
        };
        let sql = format!(
            "WITH scope AS (
                 SELECT images.id AS id,
                        CASE WHEN {pending} THEN 1 ELSE 0 END AS pending
                 FROM images {}
                 WHERE {filters}
             )
             SELECT count(*), coalesce(sum(pending), 0) FROM scope",
            prepared.join_sql
        );
        let stats = connection.query_row(&sql, params_from_iter(params.iter()), |row| {
            Ok(PendingStats {
                total: row.get::<_, i64>(0)? as u64,
                will_process: row.get::<_, i64>(1)? as u64,
            })
        })?;
        Ok(stats)
    })
}

/// 选择集查询的公共外壳：`selected_ids` 由 `json_each` 展开，`tail` 决定是聚合还是取 id。
///
/// 参数顺序固定为 JSON 数组在前、`pending` 的模型与维度在后，与 SQL 文本顺序一致。
fn selected_pending_sql(json: &str, filter: &PendingFilter, tail: &str) -> (String, Vec<Value>) {
    let mut params = vec![Value::Text(json.to_owned())];
    let pending = filter.clause(&mut params);
    let sql = format!(
        "WITH selected_ids AS (
             SELECT DISTINCT value AS id FROM json_each(?)
         ),
         scope AS (
             SELECT images.id AS id,
                    CASE WHEN {pending} THEN 1 ELSE 0 END AS pending
             FROM images
             JOIN selected_ids ON selected_ids.id = images.id
         )
         {tail}"
    );
    (sql, params)
}

/// 选择集的待处理统计；id 通过 `json_each` 传入。
///
/// 读连接是 `query_only`，不能建临时表；逐块 `IN` 在几十万张时会退化成上千条语句。
pub fn selected_pending_stats(
    database: &LibraryDb,
    ids: &[i64],
    filter: &PendingFilter,
) -> AppResult<PendingStats> {
    let ids = normalize_scope_ids(ids)?;
    if ids.is_empty() {
        return Ok(PendingStats {
            total: 0,
            will_process: 0,
        });
    }
    let json = serde_json::to_string(&ids)
        .map_err(|error| AppError::message(format!("选择集编码失败：{error}")))?;
    let (sql, params) = selected_pending_sql(
        &json,
        filter,
        "SELECT count(*), coalesce(sum(pending), 0) FROM scope",
    );
    database.with_reader(|connection| {
        let stats = connection.query_row(&sql, params_from_iter(params.iter()), |row| {
            Ok(PendingStats {
                total: row.get::<_, i64>(0)? as u64,
                will_process: row.get::<_, i64>(1)? as u64,
            })
        })?;
        Ok(stats)
    })
}

/// 选择集里仍待处理的 id，按输入顺序返回；重复 id 只保留一次。
pub fn selected_pending_ids(
    database: &LibraryDb,
    ids: &[i64],
    filter: &PendingFilter,
) -> AppResult<Vec<i64>> {
    let ids = normalize_scope_ids(ids)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let json = serde_json::to_string(&ids)
        .map_err(|error| AppError::message(format!("选择集编码失败：{error}")))?;
    let (sql, params) =
        selected_pending_sql(&json, filter, "SELECT id FROM scope WHERE pending = 1");
    let found: HashSet<i64> = database.with_reader(|connection| {
        let mut statement = connection.prepare(&sql)?;
        let rows = statement
            .query_map(params_from_iter(params.iter()), |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().collect())
    })?;
    Ok(ids.into_iter().filter(|id| found.contains(id)).collect())
}

/// 选择集 id 的公共校验：正整数、去重、数量上限；任务范围解析与待处理统计共用。
pub(crate) fn normalize_scope_ids(ids: &[i64]) -> AppResult<Vec<i64>> {
    let result = crate::library::ids::unique_positive(ids, "图片")?;
    crate::library::ids::ensure_max(
        result.len(),
        MAX_IDS,
        "单次任务最多处理 100 万张图片".to_owned(),
    )?;
    Ok(result)
}

/// 文件夹概览统计；`folder_ids` 为空表示整个图库（或当前集合）。
/// 应用全部筛选，始终排除回收站与临时区。
pub fn folder_stats(database: &LibraryDb, query: LibraryQuery) -> AppResult<FolderStats> {
    let (mut query, _) = normalize(query)?;
    if !matches!(query.view, LibraryView::All | LibraryView::Collection) {
        return Err(AppError::message("文件夹统计只在图库视图可用"));
    }
    query.sort.clear();
    query.cursor = None;
    query.page_token = None;
    query.anchor_id = None;
    query.limit = None;

    database.with_reader(|connection| {
        let transaction = connection.unchecked_transaction()?;
        let folder_paths = folder_scope_paths(&transaction, &query.filters.folder_ids)?;
        let mut prepared = PreparedQuery::new(&query, None, SearchMode::Count, &folder_paths);
        // 全库搜索不按视图加可见性条件；统计仍只算正常图片。
        prepared.require_active_images();

        let pin = pin_stats_indexes(&query);
        let facet_index = pin.then_some("idx_images_visible_facets");

        let (count_query, count_params) = count_sql(&prepared);
        let image_count = count_rows(&transaction, &count_query, &count_params)?;

        let (sum_query, sum_params) = sum_sql(&prepared, pin.then_some("idx_images_visible_bytes"));
        let total_bytes = sum_rows(&transaction, &sum_query, &sum_params)?;

        let subfolder_count = subfolder_count(&transaction, &query.filters.folder_ids)?;

        let (extension_query, extension_params) = grouped_sql(
            &prepared,
            facet_index,
            "images.file_ext, count(*)",
            "GROUP BY images.file_ext ORDER BY count(*) DESC, images.file_ext",
        );
        let extensions = grouped_values(&transaction, &extension_query, &extension_params)?;

        let (score_query, score_params) = grouped_sql(
            &prepared,
            facet_index,
            "images.score, count(*)",
            "GROUP BY images.score ORDER BY images.score DESC",
        );
        let scores = grouped_i64(&transaction, &score_query, &score_params)?
            .into_iter()
            .map(|(score, count)| ValueCount {
                value: score.to_string(),
                count,
            })
            .collect();

        let (tag_query, tag_params) = tags_grouped_sql(&prepared, facet_index);
        let tags = stats_tags(
            &transaction,
            grouped_i64(&transaction, &tag_query, &tag_params)?,
        )?;

        transaction.commit()?;
        Ok(FolderStats {
            image_count,
            total_bytes,
            subfolder_count,
            extensions,
            scores,
            tags,
        })
    })
}

/// 只在“图库视图 + 纯文件夹/标签条件”时钉住覆盖索引；搜索、日期、大小、描述与特征
/// 条件会改变最优计划，交给规划器。
fn pin_stats_indexes(query: &LibraryQuery) -> bool {
    query.view == LibraryView::All
        && query.search.is_empty()
        && query.filters.imported_from.is_none()
        && query.filters.imported_to.is_none()
        && query.filters.size_min.is_none()
        && query.filters.size_max.is_none()
        && query.filters.description == PresenceFilter::Any
        && query.filters.embedding == PresenceFilter::Any
        && query.filters.pose == PresenceFilter::Any
}

fn subfolder_count(connection: &Connection, folder_ids: &[i64]) -> AppResult<u64> {
    Ok(FolderTree::normal(connection)?.descendant_count(folder_ids))
}

fn stats_tags(connection: &Connection, counts: Vec<(i64, u64)>) -> AppResult<Vec<StatsTag>> {
    let top: Vec<(i64, u64)> = counts.into_iter().take(FOLDER_STATS_TAG_LIMIT).collect();
    if top.is_empty() {
        return Ok(Vec::new());
    }
    let marks = placeholders(top.len());
    let mut statement =
        connection.prepare(&format!("SELECT id, name FROM tags WHERE id IN ({marks})"))?;
    let rows = statement.query_map(params_from_iter(top.iter().map(|(id, _)| *id)), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut names: HashMap<i64, String> = HashMap::new();
    for row in rows {
        let (id, name) = row?;
        names.insert(id, name);
    }
    Ok(top
        .into_iter()
        .filter_map(|(id, count)| names.remove(&id).map(|name| StatsTag { id, name, count }))
        .collect())
}

/// 每个文件夹的直属图片数。单独提出来是为了让测试能断言查询计划。
const FOLDER_COUNTS_SQL: &str = "SELECT folder_id, count(*) FROM images
     WHERE deleted_at IS NULL AND purge_operation_id IS NULL GROUP BY folder_id";

pub fn list_folders(database: &LibraryDb) -> AppResult<Vec<FolderInfo>> {
    database.with_reader(|connection| {
        let tree = FolderTree::normal(connection)?;
        let mut statement = connection.prepare(FOLDER_COUNTS_SQL)?;
        let direct = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);

        let direct_by_id: HashMap<i64, u64> = direct.iter().copied().collect();
        let totals: HashMap<i64, u64> = tree.totals(direct);
        let sort_indexes: HashMap<i64, i64> = connection
            .prepare("SELECT id, sort_index FROM folders")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(tree
            .entries()
            .map(|(id, path)| FolderInfo {
                id,
                path: path.to_owned(),
                sort_index: sort_indexes.get(&id).copied().unwrap_or(0),
                direct_image_count: direct_by_id.get(&id).copied().unwrap_or(0),
                total_image_count: totals.get(&id).copied().unwrap_or(0),
            })
            .collect())
    })
}

/// 回收站文件夹树：节点计数都是回收站图片数，正常文件夹不出现在这里。
pub fn list_trash_folders(database: &LibraryDb) -> AppResult<TrashFolders> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, path, sort_index FROM folders
             WHERE kind = 'normal' AND trashed_at IS NOT NULL AND purge_operation_id IS NULL
             ORDER BY path",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);

        let mut statement = connection.prepare(
            "SELECT images.folder_id, count(*) FROM images
             JOIN folders ON folders.id = images.folder_id
             WHERE images.deleted_at IS NOT NULL AND images.purge_operation_id IS NULL
               AND folders.trashed_at IS NOT NULL AND folders.purge_operation_id IS NULL
             GROUP BY images.folder_id",
        )?;
        let direct = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);

        let direct_by_id: HashMap<i64, u64> = direct.iter().copied().collect();
        let tree = FolderTree::new(
            rows.iter()
                .map(|(id, path, _)| (*id, path.clone()))
                .collect::<Vec<_>>(),
        );
        let totals: HashMap<i64, u64> = tree.totals(direct);
        let folders: Vec<FolderInfo> = rows
            .into_iter()
            .map(|(id, path, sort_index)| FolderInfo {
                id,
                path,
                sort_index,
                direct_image_count: direct_by_id.get(&id).copied().unwrap_or(0),
                total_image_count: totals.get(&id).copied().unwrap_or(0),
            })
            .collect();
        let direct_image_count: u64 = connection.query_row(
            "SELECT count(*) FROM images
             JOIN folders ON folders.id = images.folder_id
             WHERE images.deleted_at IS NOT NULL AND images.purge_operation_id IS NULL
               AND folders.kind = 'normal' AND folders.trashed_at IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        let total_image_count = direct_image_count + totals.values().copied().sum::<u64>();
        Ok(TrashFolders {
            folders,
            direct_image_count,
            total_image_count,
        })
    })
}

/// 只返回给定 id 的最新 `FolderInfo`；用于文件夹写命令的增量返回。
///
/// 计数仍走一次全库聚合（覆盖索引后是索引内扫描），但前端只收到受影响的行。
pub fn folder_infos(database: &LibraryDb, ids: &[i64]) -> AppResult<Vec<FolderInfo>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let wanted: HashSet<i64> = ids.iter().copied().collect();
    Ok(list_folders(database)?
        .into_iter()
        .filter(|folder| wanted.contains(&folder.id))
        .collect())
}

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
const MAX_TAG_DEAD_IMAGES: u64 = 50_000;

fn list_tags_page_in(
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
fn list_tags_page_legacy(
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

fn normalize(mut query: LibraryQuery) -> AppResult<(LibraryQuery, u32)> {
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

fn sort_expression(key: SortKey) -> &'static str {
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

fn order_by(sort: &[SortSpec]) -> String {
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
fn uses_id_cursor(sort: &[SortSpec]) -> bool {
    sort.len() > 1 || sort.first().is_some_and(|spec| spec.key == SortKey::Folder)
}

fn keyset_condition(sort: &[SortSpec], values: &[i64], cursor_id: i64) -> (String, Vec<Value>) {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Facet {
    Extensions,
    Scores,
    Tags,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchMode {
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

struct PreparedQuery {
    join_sql: String,
    join_params: Vec<Value>,
    filters: FilterSet,
}

impl PreparedQuery {
    fn new(
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
    fn require_active_images(&mut self) {
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

fn build_filter_counts(
    connection: &Connection,
    query: &LibraryQuery,
    folder_paths: &[String],
) -> AppResult<FilterCounts> {
    // 搜索与集合视图的命中集会被四个 facet 分别扫描一遍，单条 MATERIALIZED CTE 只求一次。
    if query.view == LibraryView::Collection || !query.search.is_empty() {
        return build_materialized_counts(connection, query, folder_paths);
    }

    let total = {
        let prepared = PreparedQuery::new(query, None, SearchMode::Count, folder_paths);
        let (sql, params) = count_sql(&prepared);
        count_rows(connection, &sql, &params)?
    };

    let extensions = {
        let prepared = PreparedQuery::new(
            query,
            Some(Facet::Extensions),
            SearchMode::Count,
            folder_paths,
        );
        let (sql, params) = grouped_sql(
            &prepared,
            None,
            "images.file_ext, count(*)",
            "GROUP BY images.file_ext ORDER BY count(*) DESC, images.file_ext",
        );
        grouped_values(connection, &sql, &params)?
    };

    let scores = {
        let prepared =
            PreparedQuery::new(query, Some(Facet::Scores), SearchMode::Count, folder_paths);
        let (sql, params) = grouped_sql(
            &prepared,
            None,
            "images.score, count(*)",
            "GROUP BY images.score ORDER BY images.score DESC",
        );
        grouped_i64(connection, &sql, &params)?
            .into_iter()
            .map(|(score, count)| ValueCount {
                value: score.to_string(),
                count,
            })
            .collect()
    };

    let tags = {
        let prepared =
            PreparedQuery::new(query, Some(Facet::Tags), SearchMode::Count, folder_paths);
        let (sql, params) = tags_grouped_sql(&prepared, None);
        grouped_i64(connection, &sql, &params)?
            .into_iter()
            .map(|(tag_id, count)| TagCount { tag_id, count })
            .collect()
    };

    Ok(FilterCounts {
        total,
        extensions,
        scores,
        tags,
    })
}

fn build_materialized_counts(
    connection: &Connection,
    query: &LibraryQuery,
    folder_paths: &[String],
) -> AppResult<FilterCounts> {
    let base_query = LibraryQuery {
        view: query.view,
        collection_id: query.collection_id,
        search: query.search.clone(),
        search_scope: query.search_scope,
        ..Default::default()
    };
    let base = PreparedQuery::new(&base_query, None, SearchMode::Count, &[]);
    let mut params = base.join_params.clone();
    params.extend(base.filters.params.iter().cloned());

    let filters = &query.filters;
    let total = filters_only(filters, None, folder_paths);
    let extensions = filters_only(filters, Some(Facet::Extensions), folder_paths);
    let scores = filters_only(filters, Some(Facet::Scores), folder_paths);
    let tags = filters_only(filters, Some(Facet::Tags), folder_paths);

    let mut branches = Vec::with_capacity(4);
    push_count_branch(
        &mut branches,
        &mut params,
        "SELECT 'total', '0', count(*) FROM matched images",
        "",
        &total,
    );
    push_count_branch(
        &mut branches,
        &mut params,
        "SELECT 'extensions', images.file_ext, count(*) FROM matched images",
        "GROUP BY images.file_ext",
        &extensions,
    );
    push_count_branch(
        &mut branches,
        &mut params,
        "SELECT 'scores', CAST(images.score AS TEXT), count(*) FROM matched images",
        "GROUP BY images.score",
        &scores,
    );
    push_count_branch(
        &mut branches,
        &mut params,
        "SELECT 'tags', CAST(image_tags.tag_id AS TEXT), count(*)
         FROM matched images
         JOIN image_tags ON image_tags.image_id = images.id",
        "GROUP BY image_tags.tag_id",
        &tags,
    );

    let base_where = if base.filters.clauses.is_empty() {
        "1".to_owned()
    } else {
        base.filters.clauses.join(" AND ")
    };
    let mut columns = vec![
        "images.id".to_owned(),
        "images.folder_id".to_owned(),
        "images.file_ext".to_owned(),
        "images.score".to_owned(),
    ];
    if filters.imported_from.is_some() || filters.imported_to.is_some() {
        columns.push("images.imported_at".to_owned());
    }
    if filters.size_min.is_some() || filters.size_max.is_some() {
        columns.push("images.file_size".to_owned());
    }
    if filters.description != PresenceFilter::Any {
        columns.push("images.description".to_owned());
    }

    let sql = format!(
        "WITH matched AS MATERIALIZED (
             SELECT {} FROM images {} WHERE {}
         )
         {}",
        columns.join(", "),
        base.join_sql,
        base_where,
        branches.join("\nUNION ALL\n")
    );

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?.max(0) as u64,
        ))
    })?;

    let mut counts = FilterCounts {
        total: 0,
        extensions: Vec::new(),
        scores: Vec::new(),
        tags: Vec::new(),
    };
    for row in rows {
        let (kind, value, count) = row?;
        match kind.as_str() {
            "total" => counts.total = count,
            "extensions" => counts.extensions.push(ValueCount { value, count }),
            "scores" => counts.scores.push(ValueCount { value, count }),
            "tags" => counts.tags.push(TagCount {
                tag_id: parse_count_key(&value)?,
                count,
            }),
            _ => {}
        }
    }
    counts.extensions.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.value.cmp(&right.value))
    });
    counts.scores.sort_by(|left, right| {
        let left: i64 = left.value.parse().unwrap_or_default();
        let right: i64 = right.value.parse().unwrap_or_default();
        right.cmp(&left)
    });
    counts.tags.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then(left.tag_id.cmp(&right.tag_id))
    });
    Ok(counts)
}

fn filters_only(
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
fn folder_scope_paths(connection: &Connection, ids: &[i64]) -> AppResult<Vec<String>> {
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

fn push_count_branch(
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

fn parse_count_key(value: &str) -> AppResult<i64> {
    value
        .parse()
        .map_err(|_| AppError::message("筛选计数键无效"))
}

fn count_sql(prepared: &PreparedQuery) -> (String, Vec<Value>) {
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

fn sum_sql(prepared: &PreparedQuery, index: Option<&str>) -> (String, Vec<Value>) {
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

fn grouped_sql(
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

fn tags_grouped_sql(prepared: &PreparedQuery, index: Option<&str>) -> (String, Vec<Value>) {
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

fn count_rows(connection: &Connection, sql: &str, params: &[Value]) -> AppResult<u64> {
    let count: i64 =
        connection.query_row(sql, params_from_iter(params.iter()), |row| row.get(0))?;
    Ok(count.max(0) as u64)
}

fn sum_rows(connection: &Connection, sql: &str, params: &[Value]) -> AppResult<u64> {
    let sum: i64 = connection.query_row(sql, params_from_iter(params.iter()), |row| row.get(0))?;
    Ok(sum.max(0) as u64)
}

fn grouped_values(
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
    use crate::library::{folders, import::ensure_folder, service, temp};

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
                connection.execute(
                    "UPDATE images SET description = '人物速写' WHERE id = 1",
                    [],
                )?;
                connection.execute(
                    "UPDATE images SET description = '人物参考' WHERE id = 2",
                    [],
                )?;
                connection.execute(
                    "UPDATE images SET description = '风景照片' WHERE id = 3",
                    [],
                )?;
                connection.execute(
                    "UPDATE images SET description = '城市夜景' WHERE id = 4",
                    [],
                )?;
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
                sort_values: vec!["300".into()],
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
    fn filters_by_tag_presence_and_exclusions() {
        let fixture = fixture();

        let has = all_ids(
            LibraryQuery {
                filters: LibraryFilters {
                    tag_presence: PresenceFilter::Has,
                    ..Default::default()
                },
                ..Default::default()
            },
            &fixture,
        );
        assert_eq!(has, vec![4, 1, 2]);

        let missing = all_ids(
            LibraryQuery {
                filters: LibraryFilters {
                    tag_presence: PresenceFilter::Missing,
                    ..Default::default()
                },
                ..Default::default()
            },
            &fixture,
        );
        assert_eq!(missing, vec![3]);

        // 排除标签：剩下“没有速写”的图片。
        let excluded = all_ids(
            LibraryQuery {
                filters: LibraryFilters {
                    exclude_tag_ids: vec![fixture.tag_sketch],
                    ..Default::default()
                },
                ..Default::default()
            },
            &fixture,
        );
        assert_eq!(excluded, vec![4, 3]);

        // 包含与排除叠加：有速写、但没有参考 → 只剩 2。
        let combined = all_ids(
            LibraryQuery {
                filters: LibraryFilters {
                    tag_ids: vec![fixture.tag_sketch],
                    exclude_tag_ids: vec![fixture.tag_reference],
                    ..Default::default()
                },
                ..Default::default()
            },
            &fixture,
        );
        assert_eq!(combined, vec![2]);

        // “没有任何标签”与“包含某些标签”互斥。
        let conflict = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    tag_presence: PresenceFilter::Missing,
                    tag_ids: vec![fixture.tag_sketch],
                    ..Default::default()
                },
                ..base_query()
            },
        );
        assert!(conflict.is_err());
    }

    #[test]
    fn lists_tags_with_offset_and_total() {
        let fixture = fixture();
        let total = count_tags(&fixture.session.db, None).unwrap();
        assert_eq!(total, 2);

        let first = list_tags_page(&fixture.session.db, None, 1, 0).unwrap();
        assert_eq!(first.len(), 1);
        let second = list_tags_page(&fixture.session.db, None, 1, 1).unwrap();
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].id, second[0].id);
        assert!(
            list_tags_page(&fixture.session.db, None, 1, 2)
                .unwrap()
                .is_empty()
        );

        // 搜索路径与全量路径口径一致，也按 offset 切片。
        let searched = count_tags(&fixture.session.db, Some("速")).unwrap();
        assert_eq!(searched, 1);
        let page = list_tags_page(&fixture.session.db, Some("速"), 10, 0).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, fixture.tag_sketch);
    }

    #[test]
    fn sorts_by_score_and_size_and_views_trash() {
        let fixture = fixture();
        let score = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                sort: vec![SortSpec {
                    key: SortKey::Score,
                    descending: true,
                }],
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&score), vec![2, 4]);

        let size_ascending = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                sort: vec![SortSpec {
                    key: SortKey::Size,
                    descending: false,
                }],
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
    fn folder_filter_includes_descendants_but_not_similar_prefixes() {
        let fixture = fixture();
        let (nested_id, lookalike_id) = fixture
            .session
            .db
            .with_writer(|connection| {
                let nested = ensure_folder(connection, "人物/速写", 1)?;
                let lookalike = ensure_folder(connection, "人物速写", 1)?;
                insert_image(
                    connection,
                    "f",
                    "jpg",
                    2,
                    nested,
                    600,
                    6_000,
                    600,
                    600,
                    None,
                    &[],
                );
                insert_image(
                    connection,
                    "g",
                    "jpg",
                    2,
                    lookalike,
                    700,
                    7_000,
                    600,
                    600,
                    None,
                    &[],
                );
                Ok((nested, lookalike))
            })
            .unwrap();

        let by_parent = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![fixture.folder_people],
                    ..Default::default()
                },
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&by_parent), vec![6, 1, 2]);

        let by_nested = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![nested_id],
                    ..Default::default()
                },
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&by_nested), vec![6]);

        let by_lookalike = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![lookalike_id],
                    ..Default::default()
                },
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&by_lookalike), vec![7]);

        let folders = list_folders(&fixture.session.db).unwrap();
        let people = folders
            .iter()
            .find(|item| item.id == fixture.folder_people)
            .unwrap();
        assert_eq!(people.direct_image_count, 2);
        assert_eq!(people.total_image_count, 3);
        let root = folders.iter().find(|item| item.path.is_empty()).unwrap();
        assert_eq!(root.total_image_count, 6);
    }

    #[test]
    fn folder_direct_filter_excludes_descendants_and_handles_root() {
        let fixture = fixture();
        let (_nested_id, _lookalike_id) = fixture
            .session
            .db
            .with_writer(|connection| {
                let nested = ensure_folder(connection, "人物/速写", 1)?;
                let lookalike = ensure_folder(connection, "人物速写", 1)?;
                insert_image(
                    connection,
                    "f",
                    "jpg",
                    2,
                    nested,
                    600,
                    6_000,
                    600,
                    600,
                    None,
                    &[],
                );
                insert_image(
                    connection,
                    "g",
                    "jpg",
                    2,
                    lookalike,
                    700,
                    7_000,
                    600,
                    600,
                    None,
                    &[],
                );
                Ok((nested, lookalike))
            })
            .unwrap();

        let direct = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![fixture.folder_people],
                    folder_direct: true,
                    ..Default::default()
                },
                limit: Some(10),
                ..base_query()
            },
        )
        .unwrap();
        let mut direct_ids = ids(&direct);
        direct_ids.sort_unstable();
        assert_eq!(direct_ids, vec![1, 2]);

        let counts = filter_counts(
            &fixture.session.db,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![fixture.folder_people],
                    folder_direct: true,
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(counts.total, 2);

        let root_id = list_folders(&fixture.session.db)
            .unwrap()
            .into_iter()
            .find(|folder| folder.path.is_empty())
            .unwrap()
            .id;
        let root_direct = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![root_id],
                    folder_direct: true,
                    ..Default::default()
                },
                limit: Some(10),
                ..base_query()
            },
        )
        .unwrap();
        assert!(root_direct.items.is_empty());
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
        let folders = list_folders(&fixture.session.db).unwrap();
        assert_eq!(folders.len(), 3);
        assert_eq!(folders[1].path, "人物");
        assert_eq!(folders[1].direct_image_count, 2);
        assert_eq!(folders[1].total_image_count, 2);

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
                    sort: vec![SortSpec {
                        key: SortKey::Score,
                        descending: true,
                    }],
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
    fn searches_description_tags_and_folders() {
        let fixture = fixture();

        let short = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "人物".into(),
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&short), vec![1, 2]);

        let trigram = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "人物速".into(),
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&trigram), vec![1]);

        let by_tag = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "速写".into(),
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&by_tag), vec![1, 2]);

        let by_folder = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "风景".into(),
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&by_folder), vec![4, 3]);
    }

    #[test]
    fn library_scope_search_includes_trash() {
        let fixture = fixture();
        let in_view = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "速写".into(),
                search_scope: SearchScope::View,
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&in_view), vec![1, 2]);

        let whole_library = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "速写".into(),
                search_scope: SearchScope::Library,
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&whole_library), vec![5, 1]);
        assert!(whole_library.items[0].deleted_at.is_some());
    }

    #[test]
    fn filters_by_time_size_and_presence() {
        let fixture = fixture();

        let time = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    imported_from: Some(100),
                    imported_to: Some(300),
                    ..Default::default()
                },
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&time), vec![1, 2, 3]);

        let size = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    size_min: Some(2_000),
                    size_max: Some(3_000),
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&size), vec![2, 3]);

        let with_description = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    description: PresenceFilter::Has,
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&with_description), vec![4, 1]);

        let missing_embedding = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    embedding: PresenceFilter::Missing,
                    pose: PresenceFilter::Missing,
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(ids(&missing_embedding), vec![4, 1]);
    }

    #[test]
    fn views_collections_in_manual_order() {
        let fixture = fixture();
        let collection_id = fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO collections(name, sort_order, created_at, updated_at)
                     VALUES ('参考集', 0, 1, 1)",
                    [],
                )?;
                let collection_id = connection.last_insert_rowid();
                for (position, image_id) in [3_i64, 1, 4].into_iter().enumerate() {
                    connection.execute(
                        "INSERT INTO collection_items(collection_id, image_id, position)
                         VALUES (?1, ?2, ?3)",
                        params![collection_id, image_id, position as i64],
                    )?;
                }
                Ok(collection_id)
            })
            .unwrap();

        let page = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::Collection,
                collection_id: Some(collection_id),
                sort: vec![SortSpec {
                    key: SortKey::CollectionPosition,
                    descending: false,
                }],
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&page), vec![3, 1, 4]);

        let counts = filter_counts(
            &fixture.session.db,
            LibraryQuery {
                view: LibraryView::Collection,
                collection_id: Some(collection_id),
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(counts.total, 3);
        assert_eq!(
            counts
                .scores
                .iter()
                .find(|item| item.value == "1")
                .map(|item| item.count),
            Some(1)
        );
    }

    #[test]
    fn search_and_collection_counts_exclude_own_facet() {
        let fixture = fixture();

        let searched = filter_counts(
            &fixture.session.db,
            LibraryQuery {
                search: "参考".into(),
                filters: LibraryFilters {
                    folder_ids: vec![fixture.folder_people],
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(searched.total, 2);
        assert_eq!(
            searched
                .tags
                .iter()
                .find(|item| item.tag_id == fixture.tag_sketch)
                .map(|item| item.count),
            Some(2)
        );
        // 标签分面只排除标签筛选，文件夹筛选仍然生效：仅图片 1 在人物文件夹且带“参考”。
        assert_eq!(
            searched
                .tags
                .iter()
                .find(|item| item.tag_id == fixture.tag_reference)
                .map(|item| item.count),
            Some(1)
        );

        let library = filter_counts(
            &fixture.session.db,
            LibraryQuery {
                search: "速写".into(),
                search_scope: SearchScope::Library,
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(library.total, 3);

        let collection_id = fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO collections(name, sort_order, created_at, updated_at)
                     VALUES ('参考集', 0, 1, 1)",
                    [],
                )?;
                let collection_id = connection.last_insert_rowid();
                for (position, image_id) in [3_i64, 1, 4].into_iter().enumerate() {
                    connection.execute(
                        "INSERT INTO collection_items(collection_id, image_id, position)
                         VALUES (?1, ?2, ?3)",
                        params![collection_id, image_id, position as i64],
                    )?;
                }
                Ok(collection_id)
            })
            .unwrap();

        let collection = filter_counts(
            &fixture.session.db,
            LibraryQuery {
                view: LibraryView::Collection,
                collection_id: Some(collection_id),
                filters: LibraryFilters {
                    tag_ids: vec![fixture.tag_reference],
                    ..Default::default()
                },
                ..base_query()
            },
        )
        .unwrap();
        assert_eq!(collection.total, 2);
        // 标签分面排除标签筛选：集合内 1/3/4 的标签关系都要计入。
        assert_eq!(
            collection
                .tags
                .iter()
                .find(|item| item.tag_id == fixture.tag_reference)
                .map(|item| item.count),
            Some(2)
        );
        assert_eq!(
            collection
                .tags
                .iter()
                .find(|item| item.tag_id == fixture.tag_sketch)
                .map(|item| item.count),
            Some(1)
        );
    }

    #[test]
    fn rejects_invalid_query_parts() {
        let fixture = fixture();
        let result = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::Collection,
                ..base_query()
            },
        );
        assert!(result.is_err());

        let result = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                collection_id: Some(1),
                ..base_query()
            },
        );
        assert!(result.is_err());

        let result = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                sort: vec![SortSpec {
                    key: SortKey::CollectionPosition,
                    descending: false,
                }],
                ..base_query()
            },
        );
        assert!(result.is_err());

        let result = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    imported_from: Some(10),
                    imported_to: Some(5),
                    ..Default::default()
                },
                ..base_query()
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn multi_level_sort_pages_with_duplicates_across_six_levels() {
        let fixture = fixture();
        let (folder_a, folder_b) = fixture
            .session
            .db
            .with_writer(|connection| {
                Ok((
                    ensure_folder(connection, "排序测试", 1)?,
                    ensure_folder(connection, "排序测试二", 1)?,
                ))
            })
            .unwrap();
        let rows = [
            ("m1", 2, 100, 5, 10, 10, "排序测试"),
            ("m2", 2, 100, 5, 10, 10, "排序测试二"),
            ("m3", 2, 100, 5, 20, 5, "排序测试"),
            ("m4", 3, 50, 6, 10, 10, "排序测试二"),
            ("m5", 1, 200, 7, 10, 10, "排序测试"),
            ("m6", 2, 100, 4, 10, 10, "排序测试二"),
            ("m7", 3, 50, 6, 10, 10, "排序测试"),
            ("m8", 2, 100, 5, 5, 20, "排序测试二"),
            ("m9", 0, 100, 5, 10, 10, "排序测试"),
            ("m10", 5, 1, 1, 1, 1, "排序测试二"),
            ("m11", 2, 100, 5, 10, 10, "排序测试"),
            ("m12", 4, 100, 3, 16, 16, "排序测试二"),
        ];
        let mut expected: Vec<(i64, i64, i64, i64, i64, String)> = Vec::new();
        fixture
            .session
            .db
            .with_writer(|connection| {
                for (seed, score, size, imported, width, height, path) in rows {
                    let folder = if path == "排序测试" {
                        folder_a
                    } else {
                        folder_b
                    };
                    insert_image(
                        connection,
                        seed,
                        "jpg",
                        score,
                        folder,
                        size,
                        imported,
                        width,
                        height,
                        None,
                        &[],
                    );
                    let id = connection.last_insert_rowid();
                    expected.push((id, score, size, imported, width * height, path.to_owned()));
                }
                Ok(())
            })
            .unwrap();

        // 六级：评分↓、大小↑、导入↑、像素↑、修改时间↓、文件夹↓。
        let sort = vec![
            SortSpec {
                key: SortKey::Score,
                descending: true,
            },
            SortSpec {
                key: SortKey::Size,
                descending: false,
            },
            SortSpec {
                key: SortKey::ImportedAt,
                descending: false,
            },
            SortSpec {
                key: SortKey::Pixels,
                descending: false,
            },
            SortSpec {
                key: SortKey::ModifiedAt,
                descending: true,
            },
            SortSpec {
                key: SortKey::Folder,
                descending: true,
            },
        ];
        expected.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then(left.2.cmp(&right.2))
                .then(left.3.cmp(&right.3))
                .then(left.4.cmp(&right.4))
                .then(right.3.cmp(&left.3))
                .then(right.5.cmp(&left.5))
                .then(right.0.cmp(&left.0))
        });
        let expected: Vec<i64> = expected.into_iter().map(|row| row.0).collect();

        let mut page_token = None;
        let mut collected = Vec::new();
        loop {
            let page = query_page(
                &fixture.session.db,
                &fixture.root,
                LibraryQuery {
                    filters: LibraryFilters {
                        folder_ids: vec![folder_a, folder_b],
                        ..Default::default()
                    },
                    sort: sort.clone(),
                    page_token: page_token.clone(),
                    limit: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(page.next_cursor.is_none());
            collected.extend(ids(&page));
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        assert_eq!(collected.len(), rows.len());
        assert_eq!(collected, expected);
    }

    #[test]
    fn folder_sort_uses_id_cursor_and_orders_by_path() {
        let fixture = fixture();
        let query = LibraryQuery {
            sort: vec![SortSpec {
                key: SortKey::Folder,
                descending: false,
            }],
            limit: Some(2),
            ..Default::default()
        };
        let mut page_token = None;
        let mut paths = Vec::new();
        loop {
            let page = query_page(
                &fixture.session.db,
                &fixture.root,
                LibraryQuery {
                    sort: query.sort.clone(),
                    page_token: page_token.clone(),
                    limit: Some(2),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(page.next_cursor.is_none());
            paths.extend(page.items.iter().map(|item| item.folder_path.clone()));
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        // 第 5 张在回收站，活动视图只返回 4 张。
        assert_eq!(paths.len(), 4);
        assert_eq!(paths.first().map(String::as_str), Some("人物"));
        assert!(paths.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn anchor_page_starts_at_the_requested_image() {
        let fixture = fixture();
        let page = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                filters: LibraryFilters {
                    folder_ids: vec![fixture.folder_people],
                    ..Default::default()
                },
                sort: vec![SortSpec {
                    key: SortKey::Score,
                    descending: false,
                }],
                anchor_id: Some(1),
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.items.first().map(|item| item.id), Some(1));
        assert!(page.items.iter().any(|item| item.id == 2));
    }

    #[test]
    fn rejects_invalid_sort_specs() {
        let fixture = fixture();
        let spec = |key: SortKey| SortSpec {
            key,
            descending: true,
        };
        let page = |query: LibraryQuery| query_page(&fixture.session.db, &fixture.root, query);

        // 超过指标数量的级数。
        let too_many = vec![
            spec(SortKey::ImportedAt),
            spec(SortKey::ModifiedAt),
            spec(SortKey::Score),
            spec(SortKey::Size),
            spec(SortKey::Pixels),
            spec(SortKey::Folder),
            spec(SortKey::CollectionPosition),
            spec(SortKey::Score),
        ];
        let error = page(LibraryQuery {
            sort: too_many,
            ..base_query()
        })
        .unwrap_err();
        assert!(error.to_string().contains("排序最多"));

        // 同一指标重复。
        let error = page(LibraryQuery {
            sort: vec![spec(SortKey::Score), spec(SortKey::Score)],
            ..base_query()
        })
        .unwrap_err();
        assert!(error.to_string().contains("不能重复"));

        // 键集游标的排序值数量与级数不一致。
        let error = page(LibraryQuery {
            sort: vec![spec(SortKey::Score), spec(SortKey::Size)],
            cursor: Some(QueryCursor {
                sort_values: vec!["3".into()],
                id: 1,
            }),
            ..base_query()
        })
        .unwrap_err();
        assert!(error.to_string().contains("游标排序值"));

        // 锚点与分页 token 的格式。
        assert!(
            page(LibraryQuery {
                anchor_id: Some(0),
                ..base_query()
            })
            .is_err()
        );
        assert!(
            page(LibraryQuery {
                page_token: Some("missing-offset".into()),
                ..base_query()
            })
            .is_err()
        );
    }

    #[test]
    fn purge_marked_rows_are_hidden_everywhere() {
        let fixture = fixture();
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE images SET description = '已释放', purge_operation_id = 99 WHERE id = 5",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        // 普通视图不额外加 `purge_operation_id` 条件，靠 `deleted_at IS NULL` 挡住待释放行。
        let all = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::All,
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(all.items.iter().all(|row| row.id != 5));

        let trash = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::Trash,
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(trash.items.is_empty());

        // 全库搜索包含回收站，但不应重新显示已从回收站移除的待释放行。
        let search = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "已释放".into(),
                search_scope: SearchScope::Library,
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(search.items.is_empty());

        let rows = rows_by_ids(&fixture.session.db, &fixture.root, &[5, 1]).unwrap();
        assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), vec![1]);

        let (_, trash_count, _) = fixture.session.db.counts().unwrap();
        assert_eq!(trash_count, 0);
    }

    #[test]
    fn temp_images_are_hidden_from_normal_views_and_searches() {
        let fixture = fixture();
        let (temp_folder, temp_image) = fixture
            .session
            .db
            .with_writer(|connection| {
                let folder = temp::folder(connection)?.unwrap().id;
                connection.execute("INSERT INTO tags(name) VALUES ('临时标签')", [])?;
                let tag = connection.last_insert_rowid();
                insert_image(
                    connection,
                    "f",
                    "jpg",
                    0,
                    folder,
                    600,
                    6_000,
                    700,
                    700,
                    None,
                    &[tag],
                );
                let image: i64 =
                    connection.query_row("SELECT max(id) FROM images", [], |row| row.get(0))?;
                Ok((folder, image))
            })
            .unwrap();
        let registered = fixture
            .session
            .db
            .with_reader(temp::folder)
            .unwrap()
            .unwrap();
        assert_eq!(temp_folder, registered.id);

        let all = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::All,
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&all).len(), 4);
        assert!(all.items.iter().all(|row| row.id != temp_image));

        let temp_page = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::Temp,
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&temp_page), vec![temp_image]);

        let search = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                search: "临时标签".into(),
                search_scope: SearchScope::Library,
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(search.items.is_empty());

        // 临时视图里的“视图内搜索”能看到临时图片，但同一视图切到全库搜索时不算临时图片。
        let in_temp = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::Temp,
                search: "临时标签".into(),
                search_scope: SearchScope::View,
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&in_temp), vec![temp_image]);
        let library_wide = query_page(
            &fixture.session.db,
            &fixture.root,
            LibraryQuery {
                view: LibraryView::Temp,
                search: "临时标签".into(),
                search_scope: SearchScope::Library,
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(library_wide.items.is_empty());

        let tags = list_tags(&fixture.session.db, None).unwrap();
        assert!(!tags.iter().any(|tag| tag.name == "临时标签"));

        let (_, _, temp_count) = fixture.session.db.counts().unwrap();
        assert_eq!(temp_count, 1);
    }

    /// 标签管理器要看到 0 张标签；默认查询仍然过滤掉它们。
    #[test]
    fn include_empty_lists_zero_count_tags() {
        let fixture = fixture();
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute("INSERT INTO tags(name) VALUES ('空标签')", [])?;
                Ok(())
            })
            .unwrap();

        let visible = list_tags(&fixture.session.db, None).unwrap();
        assert!(
            !visible.iter().any(|tag| tag.name == "空标签"),
            "默认口径不返回 0 张标签"
        );

        let all = list_tags_with_empty(&fixture.session.db, None, true).unwrap();
        let empty = all.iter().find(|tag| tag.name == "空标签").unwrap();
        assert_eq!(empty.image_count, 0);
        // 有图片的标签仍在，且顺序按图片数降序。
        assert!(all.len() > visible.len());
        assert!(all[0].image_count >= all[all.len() - 1].image_count);

        let searched = search_tags_with_empty(&fixture.session.db, "空", Some(50), true).unwrap();
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].name, "空标签");
        assert!(
            search_tags(&fixture.session.db, "空", Some(50))
                .unwrap()
                .is_empty()
        );
    }

    /// 标签分页换形状后必须与旧 SQL 逐行一致：回收站、临时区、两者都有、死亡关系超阈值。
    #[test]
    fn list_tags_page_matches_legacy_shape() {
        let fixture = fixture();
        let compare = |database: &LibraryDb| {
            database
                .with_reader(|connection| {
                    let fast = list_tags_page_in(connection, None, 100, 0, MAX_TAG_DEAD_IMAGES)?;
                    let legacy = list_tags_page_legacy(connection, None, 100, 0)?;
                    assert_eq!(fast.len(), legacy.len());
                    for (left, right) in fast.iter().zip(legacy.iter()) {
                        assert_eq!(
                            (left.id, left.name.as_str(), left.image_count),
                            (right.id, right.name.as_str(), right.image_count)
                        );
                    }
                    Ok(())
                })
                .unwrap()
        };

        // fixture 里 id=5 已在回收站，且它带着两个标签关系。
        compare(&fixture.session.db);

        // 再加一张临时图片与它的标签：临时图片不算标签计数。
        fixture
            .session
            .db
            .with_writer(|connection| {
                let folder = temp::folder(connection)?.unwrap().id;
                connection.execute("INSERT INTO tags(name) VALUES ('临时标签')", [])?;
                let tag = connection.last_insert_rowid();
                insert_image(
                    connection,
                    "f",
                    "jpg",
                    0,
                    folder,
                    600,
                    6_000,
                    700,
                    700,
                    None,
                    &[tag],
                );
                Ok(())
            })
            .unwrap();
        compare(&fixture.session.db);

        // 死亡关系数超过阈值时回退旧形状，结果必须一致。
        fixture
            .session
            .db
            .with_reader(|connection| {
                let fast = list_tags_page_in(connection, None, 100, 0, MAX_TAG_DEAD_IMAGES)?;
                let fallback = list_tags_page_in(connection, None, 100, 0, 0)?;
                assert_eq!(
                    fast.iter()
                        .map(|tag| (tag.id, tag.image_count))
                        .collect::<Vec<_>>(),
                    fallback
                        .iter()
                        .map(|tag| (tag.id, tag.image_count))
                        .collect::<Vec<_>>()
                );
                Ok(())
            })
            .unwrap();
    }

    /// 名称搜索与分页也必须与旧形状一致。
    #[test]
    fn list_tags_page_search_and_offset_match_legacy_shape() {
        let fixture = fixture();
        fixture
            .session
            .db
            .with_reader(|connection| {
                for (search, limit, offset) in [(None, 1, 0), (None, 1, 1), (Some("速写"), 100, 0)]
                {
                    let fast =
                        list_tags_page_in(connection, search, limit, offset, MAX_TAG_DEAD_IMAGES)?;
                    let legacy = list_tags_page_legacy(connection, search, limit, offset)?;
                    assert_eq!(
                        fast.iter()
                            .map(|tag| (tag.id, tag.image_count))
                            .collect::<Vec<_>>(),
                        legacy
                            .iter()
                            .map(|tag| (tag.id, tag.image_count))
                            .collect::<Vec<_>>(),
                        "search={search:?} limit={limit} offset={offset}"
                    );
                }
                Ok(())
            })
            .unwrap();
    }

    /// 文件夹写命令只回传受影响的 id，计数口径必须与整树一致。
    #[test]
    fn folder_infos_returns_only_the_requested_ids() {
        let fixture = fixture();
        let all = list_folders(&fixture.session.db).unwrap();
        let subset = folder_infos(&fixture.session.db, &[fixture.folder_people, -1]).unwrap();
        assert_eq!(subset.len(), 1);
        let expected = all
            .iter()
            .find(|folder| folder.id == fixture.folder_people)
            .unwrap();
        assert_eq!(subset[0].path, expected.path);
        assert_eq!(subset[0].direct_image_count, expected.direct_image_count);
        assert_eq!(subset[0].total_image_count, expected.total_image_count);
        assert!(folder_infos(&fixture.session.db, &[]).unwrap().is_empty());
    }

    /// 文件夹计数聚合不能退回“按像素索引逐行回表”。
    #[test]
    fn folder_counts_plan_avoids_per_row_indexes() {
        let fixture = fixture();
        let plan = fixture
            .session
            .db
            .with_reader(|connection| {
                let mut statement =
                    connection.prepare(&format!("EXPLAIN QUERY PLAN {FOLDER_COUNTS_SQL}"))?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(3))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.join("\n"))
            })
            .unwrap();
        assert!(
            !plan.contains("idx_images_active"),
            "文件夹计数仍在逐行回表：{plan}"
        );
    }

    fn folder_query(folder_ids: Vec<i64>) -> LibraryQuery {
        LibraryQuery {
            filters: LibraryFilters {
                folder_ids,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn explain(connection: &Connection, sql: &str, params: &[Value]) -> String {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows = statement
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                row.get::<_, String>(3)
            })
            .unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n")
    }

    #[test]
    fn folder_stats_match_counts_bytes_and_distributions() {
        let fixture = fixture();
        let query = folder_query(vec![fixture.folder_people]);
        let stats = folder_stats(&fixture.session.db, query.clone()).unwrap();
        let counts = filter_counts(&fixture.session.db, query).unwrap();

        assert_eq!(stats.image_count, counts.total);
        assert_eq!(stats.image_count, 2, "回收站里的 e 不计入");
        assert_eq!(stats.total_bytes, 3_000);
        assert_eq!(stats.subfolder_count, 0);
        assert_eq!(
            stats.extensions,
            vec![
                ValueCount {
                    value: "jpg".into(),
                    count: 1
                },
                ValueCount {
                    value: "png".into(),
                    count: 1
                }
            ]
        );
        let scores: Vec<(String, u64)> = stats
            .scores
            .into_iter()
            .map(|item| (item.value, item.count))
            .collect();
        assert_eq!(scores, vec![("5".into(), 1), ("3".into(), 1)]);
        let tags: Vec<(String, u64)> = stats
            .tags
            .into_iter()
            .map(|tag| (tag.name, tag.count))
            .collect();
        assert_eq!(tags, vec![("速写".into(), 2), ("参考".into(), 1)]);
    }

    #[test]
    fn folder_stats_handle_children_direct_mode_and_union() {
        let fixture = fixture();
        let child = fixture
            .session
            .db
            .with_writer(|connection| {
                let child = ensure_folder(connection, "人物/速写", 1)?;
                insert_image(
                    connection,
                    "f",
                    "jpg",
                    2,
                    child,
                    700,
                    600,
                    800,
                    800,
                    None,
                    &[],
                );
                Ok(child)
            })
            .unwrap();

        let subtree = folder_stats(
            &fixture.session.db,
            folder_query(vec![fixture.folder_people]),
        )
        .unwrap();
        assert_eq!(subtree.image_count, 3);
        assert_eq!(subtree.total_bytes, 3_700);
        assert_eq!(subtree.subfolder_count, 1);

        let mut direct = folder_query(vec![fixture.folder_people]);
        direct.filters.folder_direct = true;
        let direct = folder_stats(&fixture.session.db, direct).unwrap();
        assert_eq!(direct.image_count, 2);
        assert_eq!(direct.total_bytes, 3_000);
        assert_eq!(direct.subfolder_count, 1);

        let union = folder_stats(
            &fixture.session.db,
            folder_query(vec![fixture.folder_people, child]),
        )
        .unwrap();
        assert_eq!(union.image_count, 3, "嵌套选择不能重复计数");
        assert_eq!(union.subfolder_count, 0, "选中的子文件夹不算子文件夹");
    }

    #[test]
    fn folder_stats_library_search_excludes_trash() {
        let fixture = fixture();
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE images SET description = '人物速写' WHERE id = 5",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let query = LibraryQuery {
            search: "人物速写".into(),
            search_scope: SearchScope::Library,
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                ..Default::default()
            },
            ..Default::default()
        };
        let stats = folder_stats(&fixture.session.db, query.clone()).unwrap();
        let counts = filter_counts(&fixture.session.db, query).unwrap();
        assert_eq!(counts.total, 2, "列表命中回收站里的 e");
        assert_eq!(stats.image_count, 1, "统计始终排除回收站");
    }

    #[test]
    fn folder_stats_limit_tags_to_eight() {
        let fixture = fixture();
        fixture
            .session
            .db
            .with_writer(|connection| {
                for name in ["甲", "乙", "丙", "丁", "戊", "己", "庚", "辛"] {
                    connection.execute("INSERT INTO tags(name) VALUES (?1)", [name])?;
                    let tag_id = connection.last_insert_rowid();
                    connection.execute(
                        "INSERT INTO image_tags(image_id, tag_id) VALUES (1, ?1)",
                        [tag_id],
                    )?;
                }
                Ok(())
            })
            .unwrap();

        let stats = folder_stats(
            &fixture.session.db,
            folder_query(vec![fixture.folder_people]),
        )
        .unwrap();
        assert_eq!(stats.tags.len(), 8);
        assert_eq!(stats.tags[0].name, "速写");
        assert_eq!(stats.tags[0].count, 2);
    }

    #[test]
    fn folder_stats_use_covering_indexes() {
        let fixture = fixture();
        let query = folder_query(vec![fixture.folder_people]);
        let (sum_plan, score_plan) = fixture
            .session
            .db
            .with_reader(|connection| {
                let folder_paths = folder_scope_paths(connection, &query.filters.folder_ids)?;
                let prepared = PreparedQuery::new(&query, None, SearchMode::Count, &folder_paths);
                assert!(pin_stats_indexes(&query));
                let (sum_sql, sum_params) = sum_sql(&prepared, Some("idx_images_visible_bytes"));
                let (score_sql, score_params) = grouped_sql(
                    &prepared,
                    Some("idx_images_visible_facets"),
                    "images.score, count(*)",
                    "GROUP BY images.score",
                );
                Ok((
                    explain(connection, &sum_sql, &sum_params),
                    explain(connection, &score_sql, &score_params),
                ))
            })
            .unwrap();
        assert!(
            sum_plan.contains("idx_images_visible_bytes"),
            "总字节没有走覆盖索引：{sum_plan}"
        );
        assert!(
            score_plan.contains("idx_images_visible_facets"),
            "评分分组没有走分面索引：{score_plan}"
        );
    }

    #[test]
    fn pin_stats_indexes_policy() {
        let mut query = folder_query(vec![1]);
        assert!(pin_stats_indexes(&query));

        query.search = "人物".into();
        assert!(!pin_stats_indexes(&query));

        query.search.clear();
        query.view = LibraryView::Trash;
        assert!(!pin_stats_indexes(&query));

        query.view = LibraryView::All;
        query.filters.size_min = Some(1);
        assert!(!pin_stats_indexes(&query));

        query.filters.size_min = None;
        query.filters.tag_ids = vec![1];
        assert!(pin_stats_indexes(&query), "标签条件仍可由分面索引覆盖");

        query.filters.description = PresenceFilter::Has;
        assert!(!pin_stats_indexes(&query));
    }

    fn set_thumbnail(fixture: &Fixture, id: i64, segment: i64, length: i64) {
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE images SET thumb_segment = ?1, thumb_length = ?2 WHERE id = ?3",
                    params![segment, length, id],
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn insert_feature(connection: &Connection, image_id: i64, kind: &str, model: &str, dim: u32) {
        connection
            .execute(
                "INSERT INTO feature_blobs(
                    image_id, kind, model, dim, pack_name, pack_offset, pack_length, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, 'test.pack', 0, 0, 0)",
                params![image_id, kind, model, dim],
            )
            .unwrap();
    }

    #[test]
    fn pending_stats_follow_scope_and_thumbnail_pointer() {
        let fixture = fixture();
        let database = &fixture.session.db;

        // 全库：5 行里 1 行在回收站；回收站视图的“将处理”必须是 0。
        let stats = pending_stats(database, LibraryQuery::default(), &PendingFilter::All).unwrap();
        assert_eq!(
            stats,
            PendingStats {
                total: 4,
                will_process: 4
            }
        );
        let trash = pending_stats(
            database,
            LibraryQuery {
                view: LibraryView::Trash,
                ..Default::default()
            },
            &PendingFilter::All,
        )
        .unwrap();
        assert_eq!(
            trash,
            PendingStats {
                total: 1,
                will_process: 0
            }
        );

        let stats =
            pending_stats(database, LibraryQuery::default(), &PendingFilter::Thumbnail).unwrap();
        assert_eq!(stats.will_process, 4);

        // 指针判定与 `thumbs::load_pointer` 一致：segment 与 length 都必须有值。
        set_thumbnail(&fixture, 1, 1, 100);
        set_thumbnail(&fixture, 2, 1, 0);
        set_thumbnail(&fixture, 3, 0, 50);
        let stats =
            pending_stats(database, LibraryQuery::default(), &PendingFilter::Thumbnail).unwrap();
        assert_eq!(stats.total, 4);
        assert_eq!(stats.will_process, 3, "只有 id=1 同时有 segment 与 length");

        let direct = LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                folder_direct: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let stats = pending_stats(database, direct, &PendingFilter::All).unwrap();
        assert_eq!(
            stats,
            PendingStats {
                total: 2,
                will_process: 2
            }
        );

        let subtree = LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_scenery],
                ..Default::default()
            },
            ..Default::default()
        };
        let stats = pending_stats(database, subtree, &PendingFilter::All).unwrap();
        assert_eq!(stats.total, 2);
    }

    #[test]
    fn pending_embedding_binds_model_and_dim_in_order() {
        let fixture = fixture();
        fixture
            .session
            .db
            .with_writer(|connection| {
                insert_feature(connection, 1, "embedding", "alpha", 4);
                insert_feature(connection, 2, "embedding", "alpha", 8);
                insert_feature(connection, 3, "embedding", "beta", 4);
                Ok(())
            })
            .unwrap();
        let database = &fixture.session.db;

        let alpha = pending_stats(
            database,
            LibraryQuery::default(),
            &PendingFilter::Embedding {
                model: "alpha".into(),
                dim: 4,
            },
        )
        .unwrap();
        assert_eq!(
            alpha,
            PendingStats {
                total: 4,
                will_process: 3
            }
        );
        let alpha8 = pending_stats(
            database,
            LibraryQuery::default(),
            &PendingFilter::Embedding {
                model: "alpha".into(),
                dim: 8,
            },
        )
        .unwrap();
        assert_eq!(alpha8.will_process, 3);
        let beta = pending_stats(
            database,
            LibraryQuery::default(),
            &PendingFilter::Embedding {
                model: "beta".into(),
                dim: 4,
            },
        )
        .unwrap();
        assert_eq!(beta.will_process, 3);

        fixture
            .session
            .db
            .with_writer(|connection| {
                insert_feature(connection, 2, "pose", "any-model", 33);
                Ok(())
            })
            .unwrap();
        let pose = pending_stats(database, LibraryQuery::default(), &PendingFilter::Pose).unwrap();
        assert_eq!(pose.will_process, 3, "姿势不按模型判断");
    }

    #[test]
    fn pending_description_respects_trim_and_source() {
        let fixture = fixture();
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE images SET description = '手写描述', description_source = 'user'
                     WHERE id = 1",
                    [],
                )?;
                connection.execute(
                    "UPDATE images SET description = '导入描述', description_source = 'import'
                     WHERE id = 2",
                    [],
                )?;
                connection.execute(
                    "UPDATE images SET description = '', description_source = 'ai' WHERE id = 3",
                    [],
                )?;
                connection.execute(
                    "UPDATE images SET description = '   ', description_source = 'user'
                     WHERE id = 4",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let database = &fixture.session.db;

        let fill = pending_stats(
            database,
            LibraryQuery::default(),
            &PendingFilter::Description { overwrite: false },
        )
        .unwrap();
        assert_eq!(fill.will_process, 2, "只补空描述");
        let redo = pending_stats(
            database,
            LibraryQuery::default(),
            &PendingFilter::Description { overwrite: true },
        )
        .unwrap();
        assert_eq!(redo.will_process, 2, "重做时跳过手写描述");
    }

    #[test]
    fn selected_pending_excludes_deleted_and_keeps_input_order() {
        let fixture = fixture();
        let database = &fixture.session.db;
        // id=5 在回收站；999 不存在；3 重复。
        let ids = [3, 1, 3, 999, 5];
        let stats = selected_pending_stats(database, &ids, &PendingFilter::All).unwrap();
        assert_eq!(
            stats,
            PendingStats {
                total: 3,
                will_process: 2
            }
        );
        let pending = selected_pending_ids(database, &ids, &PendingFilter::All).unwrap();
        assert_eq!(pending, vec![3, 1]);

        assert_eq!(
            selected_pending_stats(database, &[], &PendingFilter::All).unwrap(),
            PendingStats {
                total: 0,
                will_process: 0
            }
        );
        assert!(selected_pending_ids(database, &[0], &PendingFilter::All).is_err());
    }

    #[test]
    fn selected_pending_embedding_binds_json_before_model_and_dim() {
        let fixture = fixture();
        fixture
            .session
            .db
            .with_writer(|connection| {
                insert_feature(connection, 1, "embedding", "alpha", 4);
                Ok(())
            })
            .unwrap();
        let stats = selected_pending_stats(
            &fixture.session.db,
            &[1, 2],
            &PendingFilter::Embedding {
                model: "alpha".into(),
                dim: 4,
            },
        )
        .unwrap();
        assert_eq!(
            stats,
            PendingStats {
                total: 2,
                will_process: 1
            }
        );
        let pending = selected_pending_ids(
            &fixture.session.db,
            &[2, 1],
            &PendingFilter::Embedding {
                model: "alpha".into(),
                dim: 4,
            },
        )
        .unwrap();
        assert_eq!(pending, vec![2]);
    }

    #[test]
    fn selected_and_scope_pending_agree() {
        let fixture = fixture();
        set_thumbnail(&fixture, 1, 1, 100);
        let query = LibraryQuery {
            filters: LibraryFilters {
                folder_ids: vec![fixture.folder_people],
                ..Default::default()
            },
            ..Default::default()
        };
        let database = &fixture.session.db;
        let ids = query_ids(database, query.clone()).unwrap();
        let stats = pending_stats(database, query, &PendingFilter::Thumbnail).unwrap();
        let selected = selected_pending_ids(database, &ids, &PendingFilter::Thumbnail).unwrap();
        assert_eq!(stats.total as usize, ids.len());
        assert_eq!(stats.will_process as usize, selected.len());
    }

    fn insert_collection(fixture: &Fixture, image_ids: &[i64]) -> i64 {
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO collections(name, created_at, updated_at) VALUES ('测试', 0, 0)",
                    [],
                )?;
                let id = connection.last_insert_rowid();
                for (position, image_id) in image_ids.iter().enumerate() {
                    connection.execute(
                        "INSERT INTO collection_items(collection_id, image_id, position)
                         VALUES (?1, ?2, ?3)",
                        params![id, image_id, position as i64],
                    )?;
                }
                Ok(id)
            })
            .unwrap()
    }

    #[test]
    fn pending_stats_support_collection_scope() {
        let fixture = fixture();
        let collection_id = insert_collection(&fixture, &[4, 2]);
        let stats = pending_stats(
            &fixture.session.db,
            LibraryQuery {
                view: LibraryView::Collection,
                collection_id: Some(collection_id),
                ..Default::default()
            },
            &PendingFilter::All,
        )
        .unwrap();
        assert_eq!(
            stats,
            PendingStats {
                total: 2,
                will_process: 2
            }
        );
    }

    #[test]
    fn pending_stats_bind_pending_join_and_filter_params_in_order() {
        let fixture = fixture();
        let collection_id = insert_collection(&fixture, &[1, 2]);
        fixture
            .session
            .db
            .with_writer(|connection| {
                insert_feature(connection, 1, "embedding", "alpha", 4);
                Ok(())
            })
            .unwrap();
        // 集合里 id=1（评分 3、已有 embedding）与 id=2（评分 5）；评分筛选只要 3。
        // 三组参数按 SQL 文本顺序拼接：pending 的模型与维度、集合 id、评分筛选。
        let stats = pending_stats(
            &fixture.session.db,
            LibraryQuery {
                view: LibraryView::Collection,
                collection_id: Some(collection_id),
                filters: LibraryFilters {
                    scores: vec![3],
                    ..Default::default()
                },
                ..Default::default()
            },
            &PendingFilter::Embedding {
                model: "alpha".into(),
                dim: 4,
            },
        )
        .unwrap();
        assert_eq!(
            stats,
            PendingStats {
                total: 1,
                will_process: 0
            }
        );
    }

    #[test]
    fn group_keeps_picks_first_row_by_sort() {
        let fixture = fixture();
        let database = &fixture.session.db;
        // 夹具：id=1 评分 3、导入 300；id=2 评分 5、导入 200；id=3 评分 1、导入 100。
        let groups = vec![vec![1, 2], vec![3, 1]];

        let keeps = group_keeps(database, &groups, &DEFAULT_KEEP_SORT).unwrap();
        assert_eq!(keeps, vec![Some(2), Some(1)]);

        // 换成导入时间升序：第二组保留最早的 id=3。
        let sort = [SortSpec {
            key: SortKey::ImportedAt,
            descending: false,
        }];
        let keeps = group_keeps(database, &groups, &sort).unwrap();
        assert_eq!(keeps, vec![Some(2), Some(3)]);

        // 回收站里的图片不参与挑选。
        database
            .with_writer(|connection| {
                connection.execute("UPDATE images SET deleted_at = 1 WHERE id = 2", [])?;
                Ok(())
            })
            .unwrap();
        let keeps = group_keeps(database, &groups, &DEFAULT_KEEP_SORT).unwrap();
        assert_eq!(keeps, vec![Some(1), Some(1)]);

        // 跨集合才有意义的指标不能用来决定保留哪张。
        let bad = [SortSpec {
            key: SortKey::CollectionPosition,
            descending: true,
        }];
        assert!(group_keeps(database, &groups, &bad).is_err());

        // 空规则回落到默认值，而不是报错。
        let keeps = group_keeps(database, &groups, &[]).unwrap();
        assert_eq!(keeps, vec![Some(1), Some(1)]);
    }

    #[test]
    fn trash_view_separates_folder_trash_from_loose_images() {
        let fixture = fixture();
        // fixture 里 id=5 已单独在回收站；整目录移入后 3、4 也进回收站。
        folders::trash(&fixture.session.db, fixture.folder_scenery).unwrap();

        let mut all = all_ids(
            LibraryQuery {
                view: LibraryView::Trash,
                ..base_query()
            },
            &fixture,
        );
        all.sort_unstable();
        assert_eq!(all, vec![3, 4, 5]);

        let mut loose = all_ids(
            LibraryQuery {
                view: LibraryView::Trash,
                filters: LibraryFilters {
                    trash_loose: true,
                    ..Default::default()
                },
                ..base_query()
            },
            &fixture,
        );
        loose.sort_unstable();
        assert_eq!(loose, vec![5], "直属图片只含原文件夹仍在图库中的回收站图片");

        let mut folder = all_ids(
            LibraryQuery {
                view: LibraryView::Trash,
                filters: LibraryFilters {
                    folder_ids: vec![fixture.folder_scenery],
                    ..Default::default()
                },
                ..base_query()
            },
            &fixture,
        );
        folder.sort_unstable();
        assert_eq!(folder, vec![3, 4]);

        let trash_folders = list_trash_folders(&fixture.session.db).unwrap();
        assert_eq!(trash_folders.folders.len(), 1);
        assert_eq!(trash_folders.folders[0].path, "风景");
        assert_eq!(trash_folders.folders[0].total_image_count, 2);
        assert_eq!(trash_folders.direct_image_count, 1);
        assert_eq!(trash_folders.total_image_count, 3);
        assert!(
            list_folders(&fixture.session.db)
                .unwrap()
                .iter()
                .all(|folder| folder.path != "风景"),
            "正常文件夹列表不含回收站文件夹"
        );
    }

    #[test]
    fn subtree_counts_survive_mismatched_path_case() {
        let fixture = fixture();
        // 旧数据可能把子路径写成另一种大小写：祖先计数不能因此整段丢失。
        let child = fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO folders(path, created_at, sort_index) VALUES ('人物/速写', 1, 0)",
                    [],
                )?;
                connection.execute(
                    "INSERT INTO folders(path, created_at, sort_index) VALUES ('人物/速写/手部', 1, 0)",
                    [],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        fixture
            .session
            .db
            .with_writer(|connection| {
                insert_image(
                    connection,
                    "f",
                    "jpg",
                    4,
                    child,
                    4_000,
                    400,
                    640,
                    480,
                    None,
                    &[],
                );
                Ok(())
            })
            .unwrap();

        let folders = list_folders(&fixture.session.db).unwrap();
        let total = |path: &str| {
            folders
                .iter()
                .find(|folder| folder.path == path)
                .map(|folder| folder.total_image_count)
                .unwrap_or(0)
        };
        assert_eq!(total("人物/速写/手部"), 1);
        assert_eq!(total("人物/速写"), 1, "大小写不同也要算进祖先");
        assert_eq!(total("人物"), 3, "原有 2 张加新增 1 张");

        let stats = folder_stats(
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
        assert_eq!(stats.subfolder_count, 2, "子文件夹数按祖先关系算");
    }
}

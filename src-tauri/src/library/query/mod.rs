//! 浏览与查询：公开类型、有序 id 列表缓存与各子模块的入口重导出。

use std::sync::{Arc, Mutex};

use rusqlite::types::Value;
use serde::{Deserialize, Serialize};

use crate::app::error::{AppError, AppResult};

mod folders;
mod page;
mod pending;
mod sql;
mod stats;
mod tags;

#[cfg(test)]
mod tests;

pub(crate) use folders::normalize_scope_ids;
pub use folders::{folder_infos, list_folders, list_trash_folders};
pub use page::{query_ids, query_page};
pub use pending::{pending_stats, selected_pending_ids, selected_pending_stats};
pub use sql::rows_by_ids;
#[cfg(test)]
pub(crate) use stats::DEFAULT_KEEP_SORT;
pub use stats::{filter_counts, folder_stats, group_keeps, query_total};
pub use tags::{count_tags, list_tags_page, list_tags_with_empty, search_tags_with_empty};
#[cfg(test)]
pub(crate) use tags::{list_tags, list_tags_page_legacy_bench, search_tags};

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

use std::collections::{HashMap, HashSet};

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::{
    ai::runtime::AiRuntime,
    app::error::{AppError, AppResult},
    library::query::{
        self, LibraryFilters, LibraryQuery, LibraryView, PresenceFilter, QueryCursor, SearchScope,
        SortKey, SortSpec, TagMode,
    },
};

use super::{context::ReadContext, sheet::SheetCell};

const MAX_SELECTION_LIMIT: usize = 10_000;
const MAX_IDS: usize = MAX_SELECTION_LIMIT;
/// 写入 selector 可以覆盖整库；与界面批量任务的上限一致。
const MAX_WRITE_IDS: usize = 1_000_000;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum QueryView {
    #[default]
    All,
    Trash,
    Collection,
    Temp,
}

impl From<QueryView> for LibraryView {
    fn from(value: QueryView) -> Self {
        match value {
            QueryView::All => LibraryView::All,
            QueryView::Trash => LibraryView::Trash,
            QueryView::Collection => LibraryView::Collection,
            QueryView::Temp => LibraryView::Temp,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum QueryScope {
    #[default]
    View,
    Library,
}

impl From<QueryScope> for SearchScope {
    fn from(value: QueryScope) -> Self {
        match value {
            QueryScope::View => SearchScope::View,
            QueryScope::Library => SearchScope::Library,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum McpSortKey {
    #[default]
    ImportedAt,
    ModifiedAt,
    Score,
    Size,
    Pixels,
    Folder,
    CollectionPosition,
}

impl From<McpSortKey> for SortKey {
    fn from(value: McpSortKey) -> Self {
        match value {
            McpSortKey::ImportedAt => SortKey::ImportedAt,
            McpSortKey::ModifiedAt => SortKey::ModifiedAt,
            McpSortKey::Score => SortKey::Score,
            McpSortKey::Size => SortKey::Size,
            McpSortKey::Pixels => SortKey::Pixels,
            McpSortKey::Folder => SortKey::Folder,
            McpSortKey::CollectionPosition => SortKey::CollectionPosition,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct McpSortSpec {
    pub key: McpSortKey,
    pub descending: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpPresence {
    #[default]
    Any,
    Has,
    Missing,
}

impl From<McpPresence> for PresenceFilter {
    fn from(value: McpPresence) -> Self {
        match value {
            McpPresence::Any => PresenceFilter::Any,
            McpPresence::Has => PresenceFilter::Has,
            McpPresence::Missing => PresenceFilter::Missing,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpTagMode {
    #[default]
    Any,
    All,
}

impl From<McpTagMode> for TagMode {
    fn from(value: McpTagMode) -> Self {
        match value {
            McpTagMode::Any => TagMode::Any,
            McpTagMode::All => TagMode::All,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct McpFilters {
    #[schemars(description = "包含子树；最多 256 个文件夹 id")]
    pub folder_ids: Vec<i64>,
    #[schemars(description = "扩展名，不带点，例如 png、jpg")]
    pub extensions: Vec<String>,
    #[schemars(description = "评分 0-5")]
    pub scores: Vec<i64>,
    #[schemars(description = "标签 id；最多 64 个")]
    pub tag_ids: Vec<i64>,
    pub tag_mode: McpTagMode,
    #[schemars(
        description = "标签存在性：has 至少一个标签，missing 一个标签都没有；any 不限制。missing 不能与 tagIds 同时使用"
    )]
    pub tag_presence: McpPresence,
    #[schemars(description = "排除拥有其中任一标签的图片；最多 64 个")]
    pub exclude_tag_ids: Vec<i64>,
    pub imported_from: Option<i64>,
    pub imported_to: Option<i64>,
    pub size_min: Option<i64>,
    pub size_max: Option<i64>,
    pub description: McpPresence,
    pub embedding: McpPresence,
    pub pose: McpPresence,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpCursor {
    pub sort_values: Vec<String>,
    pub id: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct McpQuery {
    #[schemars(
        description = "all=正常图片；trash=回收站；collection=集合（需要 collectionId）；temp=临时文件夹"
    )]
    pub view: QueryView,
    #[schemars(description = "view=collection 时必填")]
    pub collection_id: Option<i64>,
    #[schemars(description = "搜索文本，最多 200 字")]
    pub search: String,
    #[schemars(description = "view=当前视图内搜索；library=全库搜索（包含回收站）")]
    pub search_scope: QueryScope,
    pub filters: McpFilters,
    #[schemars(description = "最多 7 级，指标不能重复；省略按导入时间倒序")]
    pub sort: Vec<McpSortSpec>,
    pub cursor: Option<McpCursor>,
    pub page_token: Option<String>,
    pub anchor_id: Option<i64>,
}

impl McpQuery {
    pub fn to_library_query(&self) -> LibraryQuery {
        LibraryQuery {
            view: self.view.into(),
            collection_id: self.collection_id,
            search: self.search.clone(),
            search_scope: self.search_scope.into(),
            filters: LibraryFilters {
                folder_ids: self.filters.folder_ids.clone(),
                folder_direct: false,
                extensions: self.filters.extensions.clone(),
                scores: self.filters.scores.clone(),
                tag_ids: self.filters.tag_ids.clone(),
                tag_mode: self.filters.tag_mode.into(),
                tag_presence: self.filters.tag_presence.into(),
                exclude_tag_ids: self.filters.exclude_tag_ids.clone(),
                imported_from: self.filters.imported_from,
                imported_to: self.filters.imported_to,
                size_min: self.filters.size_min,
                size_max: self.filters.size_max,
                description: self.filters.description.into(),
                embedding: self.filters.embedding.into(),
                pose: self.filters.pose.into(),
            },
            sort: self
                .sort
                .iter()
                .map(|spec| SortSpec {
                    key: spec.key.into(),
                    descending: spec.descending,
                })
                .collect(),
            cursor: self.cursor.as_ref().map(|cursor| QueryCursor {
                sort_values: cursor.sort_values.clone(),
                id: cursor.id,
            }),
            page_token: self.page_token.clone(),
            anchor_id: self.anchor_id,
            limit: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Selector {
    /// 结构化查询；cursor/pageToken 来自上一次搜索结果。
    #[serde(rename_all = "camelCase")]
    Query {
        #[serde(default)]
        query: Box<McpQuery>,
        #[serde(default)]
        exclude_ids: Vec<i64>,
    },
    /// 明确图片 id。
    #[serde(rename_all = "camelCase")]
    Ids {
        ids: Vec<i64>,
        #[serde(default)]
        exclude_ids: Vec<i64>,
    },
    /// 一张拼图的格子；cells 省略表示整盘，excludeCells 表示排除格子。
    #[serde(rename_all = "camelCase")]
    Sheet {
        sheet_id: String,
        #[serde(default)]
        cells: Vec<String>,
        #[serde(default)]
        exclude_cells: Vec<String>,
        #[serde(default)]
        exclude_ids: Vec<i64>,
    },
    /// 上一次 find_similar 返回的 token；offset/limit 在写入与读取时都按切片生效。
    #[serde(rename_all = "camelCase")]
    Similarity {
        token: String,
        #[schemars(description = "跳过结果槽里的前 offset 条")]
        #[serde(default)]
        offset: usize,
        #[schemars(description = "最多取多少条；省略表示取到结果槽末尾（最多 10000 条）")]
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        exclude_ids: Vec<i64>,
    },
}

pub struct SelectionSlot {
    pub label: Option<String>,
    pub row: Option<query::ImageRow>,
}

pub struct SelectionPage {
    pub slots: Vec<SelectionSlot>,
    pub total: u64,
    pub next: Option<Selector>,
    pub missing_ids: Vec<i64>,
}

/// 写入用的全量解析结果：不分页，按 selector 顺序去重。
pub struct IdSelection {
    pub ids: Vec<i64>,
    pub missing_ids: Vec<i64>,
}

pub fn resolve_ids(
    selector: &Selector,
    context: &ReadContext,
    ai: &AiRuntime,
) -> AppResult<IdSelection> {
    match selector {
        Selector::Query { query, exclude_ids } => {
            let ids = query::query_ids(&context.database, query.to_library_query())?;
            Ok(IdSelection {
                ids: apply_excludes(dedupe_ids(&ids, MAX_WRITE_IDS)?, exclude_ids),
                missing_ids: Vec::new(),
            })
        }
        Selector::Ids { ids, exclude_ids } => {
            let ids = apply_excludes(dedupe_ids(ids, MAX_WRITE_IDS)?, exclude_ids);
            let existing: HashSet<i64> =
                query::rows_by_ids(&context.database, &context.root, &ids)?
                    .into_iter()
                    .map(|row| row.id)
                    .collect();
            let mut resolved = Vec::with_capacity(ids.len());
            let mut missing_ids = Vec::new();
            for id in ids {
                if existing.contains(&id) {
                    resolved.push(id);
                } else {
                    missing_ids.push(id);
                }
            }
            Ok(IdSelection {
                ids: resolved,
                missing_ids,
            })
        }
        Selector::Sheet {
            sheet_id,
            cells,
            exclude_cells,
            exclude_ids,
        } => {
            let record = context.sheets.lock().get(sheet_id)?;
            let selected = select_cells(&record.cells, cells, exclude_cells)?;
            let excluded: HashSet<i64> = exclude_ids.iter().copied().collect();
            let mut ids = Vec::new();
            let mut seen = HashSet::new();
            for cell in selected {
                if let Some(id) = cell.image_id
                    && !excluded.contains(&id)
                    && seen.insert(id)
                {
                    ids.push(id);
                }
            }
            Ok(IdSelection {
                ids,
                missing_ids: Vec::new(),
            })
        }
        // 写入与读取同口径：offset 跳过前若干条，limit 决定最多取多少条。
        // limit 省略时取整个结果槽（最多 MAX_SELECTION_LIMIT 条）。
        Selector::Similarity {
            token,
            offset,
            limit,
            exclude_ids,
        } => {
            let take = limit.map(|value| value.clamp(1, MAX_SELECTION_LIMIT));
            let excluded: HashSet<i64> = exclude_ids.iter().copied().collect();
            let mut ids = Vec::new();
            let mut seen = HashSet::new();
            let mut cursor = *offset;
            loop {
                if take.is_some_and(|take| ids.len() >= take) {
                    break;
                }
                let page = ai.similarity_page(token, cursor, Some(300))?;
                let page_total = page.total;
                let page_next = page.next_offset;
                let page_len = page.items.len();
                for row in page.items {
                    if !excluded.contains(&row.id) && seen.insert(row.id) {
                        ids.push(row.id);
                        if take.is_some_and(|take| ids.len() >= take) {
                            break;
                        }
                    }
                }
                if ids.len() >= MAX_SELECTION_LIMIT {
                    break;
                }
                let next = page_next.unwrap_or(cursor + page_len);
                if next <= cursor || page_len == 0 {
                    break;
                }
                cursor = next;
                if cursor >= page_total {
                    break;
                }
            }
            ids.truncate(MAX_SELECTION_LIMIT);
            Ok(IdSelection {
                ids,
                missing_ids: Vec::new(),
            })
        }
    }
}

pub fn resolve_page(
    selector: &Selector,
    context: &ReadContext,
    ai: &AiRuntime,
    limit: usize,
) -> AppResult<SelectionPage> {
    let limit = limit.clamp(1, MAX_SELECTION_LIMIT);
    match selector {
        Selector::Query { query, exclude_ids } => {
            resolve_query_page(query, exclude_ids, context, limit)
        }
        Selector::Ids { ids, exclude_ids } => resolve_ids_page(ids, exclude_ids, context, limit),
        Selector::Sheet {
            sheet_id,
            cells,
            exclude_cells,
            exclude_ids,
        } => resolve_sheet_page(sheet_id, cells, exclude_cells, exclude_ids, context, limit),
        Selector::Similarity {
            token,
            offset,
            limit: selector_limit,
            exclude_ids,
        } => resolve_similarity_page(
            token,
            *offset,
            selector_limit.unwrap_or(limit),
            exclude_ids,
            ai,
            limit,
        ),
    }
}

fn resolve_query_page(
    input: &McpQuery,
    exclude_ids: &[i64],
    context: &ReadContext,
    limit: usize,
) -> AppResult<SelectionPage> {
    let base = input.to_library_query();
    let total = query::query_total(&context.database, base.clone())?;
    let mut query = base;
    query.limit = Some(limit as u32);
    let page = query::query_page(&context.database, &context.root, query)?;
    let excluded: HashSet<i64> = exclude_ids.iter().copied().collect();
    let slots = page
        .items
        .into_iter()
        .filter(|row| !excluded.contains(&row.id))
        .map(|row| SelectionSlot {
            label: None,
            row: Some(row),
        })
        .collect();

    let next = if page.next_page_token.is_some() || page.next_cursor.is_some() {
        let mut next_query = input.clone();
        next_query.anchor_id = None;
        if page.next_page_token.is_some() {
            next_query.page_token = page.next_page_token;
            next_query.cursor = None;
        } else {
            next_query.page_token = None;
            next_query.cursor = page.next_cursor.map(|cursor| McpCursor {
                sort_values: cursor.sort_values,
                id: cursor.id,
            });
        }
        Some(Selector::Query {
            query: Box::new(next_query),
            exclude_ids: exclude_ids.to_vec(),
        })
    } else {
        None
    };

    Ok(SelectionPage {
        slots,
        total,
        next,
        missing_ids: Vec::new(),
    })
}

fn resolve_ids_page(
    ids: &[i64],
    exclude_ids: &[i64],
    context: &ReadContext,
    limit: usize,
) -> AppResult<SelectionPage> {
    let ids = apply_excludes(dedupe_ids(ids, MAX_IDS)?, exclude_ids);
    let total = ids.len() as u64;
    let page_ids = ids.iter().take(limit).copied().collect::<Vec<_>>();
    let rows = query::rows_by_ids(&context.database, &context.root, &page_ids)?;
    let mut by_id: HashMap<i64, query::ImageRow> =
        rows.into_iter().map(|row| (row.id, row)).collect();
    let mut missing_ids = Vec::new();
    let slots = page_ids
        .iter()
        .map(|id| SelectionSlot {
            label: None,
            row: by_id.remove(id).or_else(|| {
                missing_ids.push(*id);
                None
            }),
        })
        .collect();
    let next = (ids.len() > limit).then(|| Selector::Ids {
        ids: ids[limit..].to_vec(),
        exclude_ids: Vec::new(),
    });
    Ok(SelectionPage {
        slots,
        total,
        next,
        missing_ids,
    })
}

fn resolve_sheet_page(
    sheet_id: &str,
    cells: &[String],
    exclude_cells: &[String],
    exclude_ids: &[i64],
    context: &ReadContext,
    limit: usize,
) -> AppResult<SelectionPage> {
    let record = context.sheets.lock().get(sheet_id)?;
    let selected = select_cells(&record.cells, cells, exclude_cells)?;
    let excluded: HashSet<i64> = exclude_ids.iter().copied().collect();
    let selected: Vec<&SheetCell> = selected
        .into_iter()
        .filter(|cell| cell.image_id.is_none_or(|id| !excluded.contains(&id)))
        .collect();
    let total = selected.len() as u64;
    let page = &selected[..selected.len().min(limit)];
    let ids = page
        .iter()
        .filter_map(|cell| cell.image_id)
        .collect::<Vec<_>>();
    let rows = query::rows_by_ids(&context.database, &context.root, &ids)?;
    let mut by_id: HashMap<i64, query::ImageRow> =
        rows.into_iter().map(|row| (row.id, row)).collect();
    let mut missing_ids = Vec::new();
    let slots = page
        .iter()
        .map(|cell| SelectionSlot {
            label: Some(cell.label.clone()),
            row: cell.image_id.and_then(|id| {
                by_id.remove(&id).or_else(|| {
                    missing_ids.push(id);
                    None
                })
            }),
        })
        .collect();
    let next = (selected.len() > limit).then(|| Selector::Sheet {
        sheet_id: sheet_id.to_owned(),
        cells: selected[limit..]
            .iter()
            .map(|cell| cell.label.clone())
            .collect(),
        exclude_cells: Vec::new(),
        exclude_ids: Vec::new(),
    });
    Ok(SelectionPage {
        slots,
        total,
        next,
        missing_ids,
    })
}

fn resolve_similarity_page(
    token: &str,
    offset: usize,
    selector_limit: usize,
    exclude_ids: &[i64],
    ai: &AiRuntime,
    limit: usize,
) -> AppResult<SelectionPage> {
    let limit = selector_limit.min(limit).max(1);
    let excluded: HashSet<i64> = exclude_ids.iter().copied().collect();
    let mut cursor = offset;
    let mut slots = Vec::new();
    let mut total = 0_u64;
    while slots.len() < limit {
        let page_start = cursor;
        let remaining = limit - slots.len() + excluded.len();
        let page = ai.similarity_page(token, cursor, Some(remaining.min(300)))?;
        total = page.total as u64;
        let page_len = page.items.len();
        let mut consumed = 0;
        let mut page_next = page.next_offset;
        for (index, row) in page.items.into_iter().enumerate() {
            consumed = index + 1;
            if !excluded.contains(&row.id) {
                slots.push(SelectionSlot {
                    label: None,
                    row: Some(row),
                });
                if slots.len() == limit {
                    break;
                }
            }
        }
        if slots.len() == limit {
            // 只消费了当前页的一部分；下一页从实际消费位置继续，避免重复。
            cursor = page_start + consumed;
            break;
        }
        match page_next.take() {
            Some(next) if next > cursor => cursor = next,
            _ => {
                cursor = page_start + page_len;
                break;
            }
        }
    }
    let has_more = (slots.len() as u64) < total && cursor < total as usize;
    let next = has_more.then(|| Selector::Similarity {
        token: token.to_owned(),
        offset: cursor,
        limit: Some(limit),
        exclude_ids: exclude_ids.to_vec(),
    });
    Ok(SelectionPage {
        slots,
        total,
        next,
        missing_ids: Vec::new(),
    })
}

fn select_cells<'a>(
    cells: &'a [SheetCell],
    requested: &[String],
    excluded: &[String],
) -> AppResult<Vec<&'a SheetCell>> {
    let excluded: HashSet<String> = excluded
        .iter()
        .map(|label| normalize_label(label))
        .collect();
    let by_label: HashMap<&str, &SheetCell> = cells
        .iter()
        .map(|cell| (cell.label.as_str(), cell))
        .collect();
    let selected = if requested.is_empty() {
        cells.iter().collect::<Vec<_>>()
    } else {
        requested
            .iter()
            .map(|label| {
                let label = normalize_label(label);
                by_label
                    .get(label.as_str())
                    .copied()
                    .ok_or_else(|| AppError::message(format!("拼图里没有格子 {label}")))
            })
            .collect::<AppResult<Vec<_>>>()?
    };
    Ok(selected
        .into_iter()
        .filter(|cell| !excluded.contains(&cell.label))
        .collect())
}

fn normalize_label(label: &str) -> String {
    label.trim().to_ascii_uppercase()
}

/// 校验 id 合法性并去重，保留输入顺序；`max` 是单次调用的上限。
pub fn dedupe_ids(ids: &[i64], max: usize) -> AppResult<Vec<i64>> {
    if ids.len() > max {
        return Err(AppError::message(format!(
            "一次最多 {max} 张图片，收到 {} 张",
            ids.len()
        )));
    }
    let mut seen = HashSet::with_capacity(ids.len());
    let mut result = Vec::with_capacity(ids.len());
    for id in ids {
        if *id <= 0 {
            return Err(AppError::message(format!("图片 id 无效：{id}")));
        }
        if seen.insert(*id) {
            result.push(*id);
        }
    }
    Ok(result)
}

fn apply_excludes(ids: Vec<i64>, exclude_ids: &[i64]) -> Vec<i64> {
    if exclude_ids.is_empty() {
        return ids;
    }
    let excluded: HashSet<i64> = exclude_ids.iter().copied().collect();
    ids.into_iter()
        .filter(|id| !excluded.contains(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_query_fields_to_library_query() {
        let input = McpQuery {
            view: QueryView::Collection,
            collection_id: Some(7),
            search: " 人物 ".into(),
            search_scope: QueryScope::Library,
            filters: McpFilters {
                folder_ids: vec![1, 2],
                extensions: vec!["PNG".into()],
                scores: vec![4, 5],
                tag_ids: vec![3],
                tag_mode: McpTagMode::All,
                tag_presence: McpPresence::Has,
                exclude_tag_ids: vec![8],
                imported_from: Some(1),
                imported_to: Some(2),
                size_min: Some(10),
                size_max: Some(20),
                description: McpPresence::Has,
                embedding: McpPresence::Missing,
                pose: McpPresence::Any,
            },
            sort: vec![McpSortSpec {
                key: McpSortKey::Score,
                descending: false,
            }],
            cursor: None,
            page_token: None,
            anchor_id: Some(9),
        };
        let query = input.to_library_query();
        assert!(matches!(query.view, LibraryView::Collection));
        assert_eq!(query.collection_id, Some(7));
        assert_eq!(query.search, " 人物 ");
        assert!(matches!(query.search_scope, SearchScope::Library));
        assert_eq!(query.filters.folder_ids, vec![1, 2]);
        assert!(matches!(query.filters.tag_mode, TagMode::All));
        assert_eq!(query.filters.tag_presence, PresenceFilter::Has);
        assert_eq!(query.filters.exclude_tag_ids, vec![8]);
        assert!(matches!(query.filters.description, PresenceFilter::Has));
        assert_eq!(query.sort.len(), 1);
        assert!(matches!(query.sort[0].key, SortKey::Score));
        assert_eq!(query.anchor_id, Some(9));
    }

    #[test]
    fn dedupes_and_rejects_invalid_ids() {
        assert_eq!(dedupe_ids(&[3, 1, 3, 2], MAX_IDS).unwrap(), vec![3, 1, 2]);
        assert!(dedupe_ids(&[0], MAX_IDS).is_err());
        assert!(dedupe_ids(&[-1], MAX_IDS).is_err());
        assert!(dedupe_ids(&[1, 2, 3], 2).is_err());
    }
}

use std::collections::HashMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ai::runtime::AiRuntime,
    app::{
        error::{AppError, AppResult},
        settings::McpMode,
        tasks::TaskScope,
    },
    imageops::contact_sheet,
    library::{
        capability, collections, metadata,
        paths::{normalize_folder, parent_path},
        query::{self, ImageRow},
        similarity::MAX_REFERENCE_IDS,
    },
};

use crate::mcp::{
    context::ReadContext,
    selection::{self, McpCursor, McpQuery, Selector},
    sheet::{self, SHEET_TTL_MINUTES, SheetResolution},
};

use super::feature::MAX_TEMP_FEATURE_IDS;

pub const MAX_SEARCH_PAGE: u32 = 50;
pub const MAX_IMAGE_IDS: usize = 50;
pub const MAX_PREVIEW: usize = 25;
/// 列表工具（`tags_list` / `folders_list`）的单页上限；分页只让超出的内容可达，不放宽单页。
pub const MAX_LIST_PAGE: u32 = 500;

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchImagesInput {
    /// 结构化查询；字段与浏览视图的筛选一致。
    pub query: McpQuery,
    /// 覆盖 query 内的游标。
    pub cursor: Option<McpCursor>,
    /// 覆盖 query 内的 id 列表游标。
    pub page_token: Option<String>,
    #[schemars(description = "每页张数，最多 50；默认 50")]
    pub page_size: Option<u32>,
    #[schemars(description = "同时返回文件夹/扩展名/评分/标签分面计数；较慢，按需开启")]
    pub include_facets: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FoldersListInput {
    /// 只列这个文件夹的直接子文件夹；省略时列根目录。
    pub parent_path: Option<String>,
    #[schemars(description = "最多返回数量，1-500；默认 200")]
    pub limit: Option<u32>,
    #[schemars(description = "上一页返回的 nextCursor；首页省略")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TagsListInput {
    /// 按名称模糊匹配；省略时按图片数排序列出。
    pub query: Option<String>,
    #[schemars(description = "最多返回数量，1-500；默认 100")]
    pub limit: Option<u32>,
    #[schemars(description = "上一页返回的 nextCursor；首页省略")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImagesGetInput {
    #[schemars(description = "图片 id，一次最多 50 张")]
    pub ids: Vec<i64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SimilarMode {
    #[default]
    Image,
    Text,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum FindSimilarScope {
    Library,
    Temp,
    #[serde(rename_all = "camelCase")]
    Folder {
        folder_id: i64,
        #[serde(default = "default_include_subfolders")]
        include_subfolders: bool,
    },
    #[serde(rename_all = "camelCase")]
    Collection {
        collection_id: i64,
    },
    #[serde(rename_all = "camelCase")]
    Selection {
        image_ids: Vec<i64>,
    },
    #[serde(rename_all = "camelCase")]
    View {
        query: Box<McpQuery>,
    },
}

fn default_include_subfolders() -> bool {
    true
}

impl FindSimilarScope {
    fn to_task_scope(&self) -> TaskScope {
        match self {
            Self::Library => TaskScope::Library,
            Self::Temp => TaskScope::Temp,
            Self::Folder {
                folder_id,
                include_subfolders,
            } => TaskScope::Folder {
                folder_id: *folder_id,
                include_subfolders: *include_subfolders,
            },
            Self::Collection { collection_id } => TaskScope::Collection {
                collection_id: *collection_id,
            },
            Self::Selection { image_ids } => TaskScope::Selected {
                image_ids: image_ids.clone(),
            },
            Self::View { query } => TaskScope::View {
                query: Box::new(query.to_library_query()),
            },
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct FindSimilarInput {
    #[schemars(description = "image 用目标图找相似；text 用语义搜索")]
    pub mode: SimilarMode,
    #[schemars(description = "mode=image 时的单张参考图；与 targetIds 二选一")]
    pub target_id: Option<i64>,
    #[schemars(
        description = "mode=image 时的多张参考图；多张只支持 color4/color8/embedding，最多 100 张"
    )]
    pub target_ids: Option<Vec<i64>>,
    #[schemars(description = "mode=text 时必填")]
    pub text: Option<String>,
    #[schemars(description = "phash / color4 / color8 / embedding / pose；默认 phash")]
    pub kind: Option<String>,
    #[schemars(
        description = "pHash 用 0-256 距离上限，省略时 6；其它指标用 -1 到 1 的分数下限，省略不限制"
    )]
    pub threshold: Option<f64>,
    #[schemars(description = "最多返回结果数，1-10000")]
    pub limit: Option<u32>,
    #[schemars(description = "返回多少条预览，1-25；默认 12")]
    pub preview_limit: Option<u32>,
    #[schemars(description = "扫描范围；默认整个图库（不含临时文件夹），type=temp 只看临时区")]
    pub scope: Option<FindSimilarScope>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImageSheetInput {
    /// 图片来源；query 用游标翻页，similarity 用 offset 翻页。
    pub selector: Selector,
    #[schemars(description = "1-5，默认 4；5×5 只用于粗筛")]
    #[serde(default)]
    pub grid: Option<u8>,
    #[schemars(description = "thumb 读缩略图包；detail 直接解码原图，最多 3×3")]
    #[serde(default)]
    pub resolution: Option<String>,
}

pub struct SheetToolResult {
    pub mapping: Value,
    pub image: Vec<u8>,
}

pub fn library_info(context: &ReadContext, tool_count: usize) -> AppResult<Value> {
    let (image_count, trash_count, temp_count) = context.database.counts()?;
    let temp_path = context.database.with_reader(crate::library::temp::path)?;
    let features = capability::feature_summary(&context.database)?;
    let (temp_embeddings, temp_poses) = capability::temp_feature_summary(&context.database)?;
    Ok(json!({
        "library": {
            "id": context.info.id,
            "name": context.info.name,
            "path": context.info.path,
            "schemaVersion": context.info.schema_version,
        },
        "counts": {
            "images": image_count,
            "trash": trash_count,
            "temp": temp_count,
        },
        "tempFolder": {
            "available": temp_path.is_some(),
            // 内部路径只用于诊断；查询临时图片用 view=temp，不要把它当普通文件夹。
            "path": temp_path,
            "count": temp_count,
            "note": "只能通过 import_start(target=temp) 进入；关闭、切换图库或退出应用时清空，不写操作记录。",
        },
        "features": {
            "descriptions": features.description,
            "embeddings": features.embedding,
            "poses": features.pose,
            "phash": features.phash,
            "colors": features.colors,
            "tempEmbeddings": temp_embeddings,
            "tempPoses": temp_poses,
        },
        "ai": {
            "baseUrl": context.settings.ai.base_url,
            "autoStart": context.settings.ai.auto_start,
            "launchConfigured": context.settings.ai.launch_configured(),
        },
        "mcp": mcp_limits(context, tool_count),
    }))
}

/// 当前档位下的权限事实。阈值只在询问档生效，其它档给 `null`，
/// 避免 AI 把 `threshold` 当成全局规则。
fn mcp_limits(context: &ReadContext, tool_count: usize) -> Value {
    let mode = context.settings.mcp.mode;
    let ask = mode == McpMode::Ask;
    json!({
        // 工具按当前设置实时判定，这里返回的就是本次调用的实际档位。
        "mode": mode.as_str(),
        "toolCount": tool_count,
        // 三档都注册写工具，客户端缓存的 tools/list 不会因为切档失效。
        "writeTools": true,
        "writeAllowed": mode != McpMode::ReadOnly,
        "approval": {
            "requiredInMode": "ask",
            "appliesToCurrentMode": ask,
            "threshold": ask.then_some(super::write::APPROVAL_THRESHOLD),
            "alwaysApproveInAsk": [
                "import_start（target=library）",
                "images_trash",
                "history_undo",
                "history_redo",
                "folders_modify（move / delete）",
                "tags_modify（delete / merge）",
            ],
            "queueClearedOnFull": true,
            "timeoutMinutes": crate::mcp::approval::ttl().as_secs() / 60,
        },
        "limits": {
            "maxSearchPage": MAX_SEARCH_PAGE,
            "maxSheetCells": u32::from(contact_sheet::MAX_GRID).pow(2),
            "maxImageIdsPerGet": MAX_IMAGE_IDS,
            "maxReferenceImages": MAX_REFERENCE_IDS,
            "maxImportPaths": 10_000,
            "maxImportFiles": 100_000,
            "maxTempFeatureImages": MAX_TEMP_FEATURE_IDS,
            "maxListPage": MAX_LIST_PAGE,
            "sheetTtlMinutes": SHEET_TTL_MINUTES,
            "importCompresses": false,
        },
    })
}

pub fn folders_list(context: &ReadContext, input: &FoldersListInput) -> AppResult<Value> {
    let folders = query::list_folders(&context.database)?;
    let parent = input
        .parent_path
        .as_deref()
        .map(normalize_folder)
        .transpose()?
        .unwrap_or_default();
    if !parent.is_empty() && !folders.iter().any(|folder| folder.path == parent) {
        return Err(AppError::message(format!("文件夹不存在：{parent}")));
    }
    let limit = input.limit.unwrap_or(200).clamp(1, MAX_LIST_PAGE) as usize;
    let offset = parse_cursor(input.cursor.as_deref())? as usize;
    let mut children: Vec<_> = folders
        .into_iter()
        .filter(|folder| parent_path(&folder.path) == parent && folder.path != parent)
        .collect();
    children.sort_by(|left, right| {
        left.sort_index
            .cmp(&right.sort_index)
            .then_with(|| left.path.to_lowercase().cmp(&right.path.to_lowercase()))
    });
    let total = children.len();
    let next_cursor = (offset + limit < total).then(|| (offset + limit).to_string());
    let items: Vec<Value> = children
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|folder| {
            json!({
                "id": folder.id,
                "path": folder.path,
                "name": folder.path.rsplit('/').next().unwrap_or(&folder.path),
                "directCount": folder.direct_image_count,
                "totalCount": folder.total_image_count,
                "sortIndex": folder.sort_index,
            })
        })
        .collect();
    Ok(json!({
        "parentPath": parent,
        "total": total,
        "folders": items,
        "nextCursor": next_cursor,
        "pagingNote": PAGE_NOTE,
    }))
}

/// 游标是不透明的十进制偏移量；解析失败直接报错，不静默回到第一页。
fn parse_cursor(cursor: Option<&str>) -> AppResult<u32> {
    match cursor.map(str::trim).filter(|text| !text.is_empty()) {
        None => Ok(0),
        Some(text) => text
            .parse::<u32>()
            .map_err(|_| AppError::message(format!("游标无效：{text}"))),
    }
}

const PAGE_NOTE: &str =
    "把 nextCursor 原样传给 cursor 取下一页；两次翻页之间有写入时可能重复或漏掉条目。";

pub fn tags_list(context: &ReadContext, input: &TagsListInput) -> AppResult<Value> {
    let query_text = input.query.as_deref().unwrap_or("").trim();
    let search = (!query_text.is_empty()).then_some(query_text);
    let limit = input.limit.unwrap_or(100).clamp(1, MAX_LIST_PAGE);
    let offset = parse_cursor(input.cursor.as_deref())?;
    let tags = query::list_tags_page(&context.database, search, limit, offset)?;
    let total = query::count_tags(&context.database, search)?;
    let next_cursor = (u64::from(offset) + u64::from(limit) < total)
        .then(|| (u64::from(offset) + u64::from(limit)).to_string());
    Ok(json!({
        "query": query_text,
        "total": total,
        "tags": tags.iter().map(|tag| json!({
            "id": tag.id,
            "name": tag.name,
            "imageCount": tag.image_count,
        })).collect::<Vec<_>>(),
        "nextCursor": next_cursor,
        "pagingNote": PAGE_NOTE,
    }))
}

pub fn collections_list(context: &ReadContext) -> AppResult<Value> {
    let collections = collections::list(&context.database)?;
    Ok(json!({
        "collections": collections.iter().map(|collection| json!({
            "id": collection.id,
            "name": collection.name,
            "imageCount": collection.image_count,
            "sortOrder": collection.sort_order,
            "createdAt": collection.created_at,
            "updatedAt": collection.updated_at,
        })).collect::<Vec<_>>(),
    }))
}

pub fn search_images(context: &ReadContext, input: &SearchImagesInput) -> AppResult<Value> {
    let base = input.query.to_library_query();
    let mut query = base.clone();
    if input.cursor.is_some() {
        query.cursor = input.cursor.as_ref().map(|cursor| query::QueryCursor {
            sort_values: cursor.sort_values.clone(),
            id: cursor.id,
        });
    }
    if input.page_token.is_some() {
        query.page_token = input.page_token.clone();
    }
    let page_size = input.page_size.unwrap_or(MAX_SEARCH_PAGE);
    if page_size == 0 || page_size > MAX_SEARCH_PAGE {
        return Err(AppError::message(format!(
            "pageSize 必须在 1-{MAX_SEARCH_PAGE} 之间，收到 {page_size}"
        )));
    }
    query.limit = Some(page_size);

    // 分面计数里已经算过同一个命中集的总数，开了分面就不再单独 count 一次。
    let facets = if input.include_facets.unwrap_or(false) {
        Some(query::filter_counts(&context.database, base.clone())?)
    } else {
        None
    };
    let total = match &facets {
        Some(facets) => facets.total,
        None => query::query_total(&context.database, base)?,
    };
    let page = query::query_page(&context.database, &context.root, query)?;
    let temp_path = temp_folder_path(context)?;
    let mut value = json!({
        "total": total,
        "pageSize": page_size,
        "images": page
            .items
            .iter()
            .map(|row| compact_row(row, temp_path.as_deref()))
            .collect::<Vec<_>>(),
        "nextCursor": page.next_cursor,
        "nextPageToken": page.next_page_token,
    });
    if let Some(facets) = facets {
        value["facets"] = json!(facets);
        value["facetsNote"] = json!(FACETS_NOTE);
    }
    Ok(value)
}

/// 分面计数对自身维度排除当前筛选是标准做法；不写清会被当成 bug。
/// 搜索与集合视图用命中集统计分面，标签分面因此只含命中集内的标签——这一句也要写进去，
/// 否则 AI 会拿“标签分面应该自排除”去解释那边的数字。
const FACETS_NOTE: &str = "标签分面不应用 tagIds、tagPresence 与 excludeTagIds：选中一个标签后仍能看到其它标签的计数。文件夹、扩展名、评分分面各自排除自身维度，其余筛选照常生效。搜索与集合视图的分面按命中集统计，标签分面只含命中集内的标签。分面只给 tagId 与计数，标签名用 tags_list 取。";

pub fn find_similar(
    context: &ReadContext,
    ai: &AiRuntime,
    input: &FindSimilarInput,
) -> AppResult<Value> {
    let limit = input.limit.unwrap_or(2000).clamp(1, 10_000) as usize;
    let preview_limit = input
        .preview_limit
        .unwrap_or(12)
        .clamp(1, MAX_PREVIEW as u32) as usize;
    let task_scope = input
        .scope
        .as_ref()
        .map(FindSimilarScope::to_task_scope)
        .unwrap_or(TaskScope::Library);
    let scope_label = task_scope.label();
    let (token, kind, returned, total_hits, elapsed_ms, extra) = match input.mode {
        SimilarMode::Image => {
            let target_ids = match (&input.target_ids, input.target_id) {
                (Some(ids), None) => ids.clone(),
                (Some(_), Some(_)) => {
                    return Err(AppError::message("targetId 与 targetIds 只能传一个"));
                }
                (None, Some(id)) => vec![id],
                (None, None) => {
                    return Err(AppError::message(
                        "mode=image 时必须提供 targetId 或 targetIds",
                    ));
                }
            };
            let kind = input.kind.as_deref().unwrap_or("phash").trim();
            if !matches!(kind, "phash" | "color4" | "color8" | "embedding" | "pose") {
                return Err(AppError::message(format!(
                    "相似度类型只能是 phash、color4、color8、embedding 或 pose，收到 {kind}"
                )));
            }
            if target_ids.len() > MAX_REFERENCE_IDS {
                return Err(AppError::message(format!(
                    "参考图最多 {MAX_REFERENCE_IDS} 张"
                )));
            }
            if target_ids.len() > 1 && !matches!(kind, "color4" | "color8" | "embedding") {
                return Err(AppError::message(
                    "多张参考图只支持 color4、color8 或 embedding",
                ));
            }
            let result = ai.find_similar_images(
                kind,
                &target_ids,
                input.threshold,
                Some(limit),
                Some(&task_scope),
            )?;
            (
                result.token,
                result.kind,
                result.total,
                result.total_hits,
                result.elapsed_ms,
                json!({
                    "targetIds": target_ids,
                    "referenceCount": target_ids.len(),
                }),
            )
        }
        SimilarMode::Text => {
            let text = input
                .text
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| AppError::message("mode=text 时必须提供 text"))?;
            let result = ai.semantic_search(
                &context.settings.ai,
                text,
                Some(limit),
                Some(&task_scope),
                input.threshold,
                None,
            )?;
            (
                result.token,
                "semantic".to_owned(),
                result.total,
                result.total_hits,
                result.elapsed_ms,
                json!({
                    "model": result.model,
                    "dim": result.dim,
                    "meanCount": result.mean_count,
                }),
            )
        }
    };
    let temp_path = temp_folder_path(context)?;
    let preview = ai.similarity_page(&token, 0, Some(preview_limit))?;
    let scores = ai.similarity_scores(&token)?;
    let score_kind = if matches!(kind.as_str(), "phash" | "color4" | "color8") {
        "distance"
    } else {
        "similarity"
    };
    Ok(json!({
        "token": token,
        "mode": match input.mode {
            SimilarMode::Image => "image",
            SimilarMode::Text => "text",
        },
        "kind": kind,
        "scoreKind": score_kind,
        "scope": scope_label,
        "total": total_hits,
        "returned": returned,
        "limit": limit,
        "elapsedMs": elapsed_ms,
        "preview": preview.items.iter().map(|row| {
            let mut value = compact_row(row, temp_path.as_deref());
            value["score"] = match scores.get(&row.id) {
                Some(score) => json!(((f64::from(*score)) * 1e6).round() / 1e6),
                None => Value::Null,
            };
            value
        }).collect::<Vec<_>>(),
        "nextOffset": preview.next_offset,
        "detail": extra,
        "note": "total 是范围与阈值过滤后的全部命中数，returned 是可翻页/可全选的条数（受 limit 截断）；phash/color 的 score 是距离，越小越近，embedding/pose/semantic 的 score 是相似度，越大越近。多张参考图只用于 color4/color8/embedding 的平均。结果只有一个结果槽；下一次 find_similar 会让这个 token 失效。",
    }))
}

pub fn images_get(context: &ReadContext, ids: &[i64]) -> AppResult<Value> {
    let ids = dedupe_ids(ids)?;
    let rows = query::rows_by_ids(&context.database, &context.root, &ids)?;
    let existing: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let capabilities = capability::image_capabilities(&context.database, &existing)?
        .into_iter()
        .map(|item| (item.image_id, item))
        .collect::<HashMap<_, _>>();
    let temp_path = temp_folder_path(context)?;
    let mut images = Vec::with_capacity(existing.len());
    let mut missing_ids = Vec::new();
    for id in &ids {
        let Some(flags) = capabilities.get(id).copied() else {
            missing_ids.push(*id);
            continue;
        };
        let details = metadata::image_details(&context.database, &context.root, *id)?;
        let temporary = temp_path
            .as_deref()
            .is_some_and(|path| details.folder_path == path);
        images.push(json!({
            "id": details.id,
            "contentHash": details.content_hash,
            "filePath": details.file_path,
            "folderId": details.folder_id,
            "folderPath": display_folder(&details.folder_path, temporary),
            "temporary": temporary,
            "fileSize": details.file_size,
            "width": details.width,
            "height": details.height,
            "format": details.format,
            "fileExt": details.file_ext,
            "hasAlpha": details.has_alpha,
            "score": details.score,
            "description": details.description,
            "descriptionSource": details.description_source,
            "descriptionModel": details.description_model,
            "importedAt": details.imported_at,
            "modifiedAt": details.modified_at,
            "deletedAt": details.deleted_at,
            "tags": details.tags.iter().map(|tag| json!({
                "id": tag.id,
                "name": tag.name,
            })).collect::<Vec<_>>(),
            "capabilities": {
                "pose": flags.pose,
                "embedding": flags.embedding,
                "colors": flags.colors,
                "phash": flags.phash,
            },
        }));
    }
    Ok(json!({
        "images": images,
        "missingIds": missing_ids,
    }))
}

pub fn image_sheet(
    context: &ReadContext,
    ai: &AiRuntime,
    input: &ImageSheetInput,
) -> AppResult<SheetToolResult> {
    let grid = input.grid.unwrap_or(4);
    let resolution = SheetResolution::parse(input.resolution.as_deref().unwrap_or("thumb"))?;
    // 先校验格数与清晰度，再解析 selector；无效组合不必白查一遍图片。
    sheet::validate(grid, resolution)?;

    let limit = (grid as usize) * (grid as usize);
    let selection = selection::resolve_page(&input.selector, context, ai, limit)?;

    let source_labels: Vec<Option<String>> = selection
        .slots
        .iter()
        .map(|slot| slot.label.clone())
        .collect();
    let labels = sheet::labels(grid);
    let mut slots = Vec::with_capacity(limit);
    for (index, label) in labels.into_iter().enumerate() {
        slots.push(match selection.slots.get(index) {
            // 输出标签必须对应新拼图的位置；来源格子只作为 sourceLabel 保留。
            Some(slot) => sheet::SheetSlot {
                label,
                row: slot.row.clone(),
            },
            None => sheet::SheetSlot { label, row: None },
        });
    }
    let mut built = sheet::build(context, grid, resolution, slots)?;
    for (index, cell) in built.cells.iter_mut().enumerate() {
        if let Some(source) = source_labels.get(index).and_then(Option::as_ref)
            && source != &cell.label
        {
            cell.source_label = Some(source.clone());
        }
    }
    // 被 excludeCells 剔掉的格子不会出现在 cells 里；单独列出来，别和“本来没图”混在一起。
    let excluded_cells = excluded_labels(context, &input.selector);
    let mapping = json!({
        "sheetId": built.record.id,
        "grid": grid,
        "resolution": resolution.as_str(),
        "total": selection.total,
        "next": selection.next,
        "missingIds": selection.missing_ids,
        "excludedCells": excluded_cells,
        "cells": built.cells,
        "note": "cells[].status：image=有图，missing=有图但缩略图/原图读不出来，empty=这一格没有图；被 excludeCells 排除的格子在 excludedCells 里，不占 cells。",
    });
    Ok(SheetToolResult {
        mapping,
        image: built.image,
    })
}

/// 从来源拼图里找出真正被 `excludeCells` 排除掉的格子；没被排除的标签不算。
fn excluded_labels(context: &ReadContext, selector: &Selector) -> Vec<String> {
    let Selector::Sheet {
        sheet_id,
        exclude_cells,
        ..
    } = selector
    else {
        return Vec::new();
    };
    if exclude_cells.is_empty() {
        return Vec::new();
    }
    let Ok(record) = context.sheets.lock().get(sheet_id) else {
        return Vec::new();
    };
    let wanted: std::collections::HashSet<String> = exclude_cells
        .iter()
        .map(|label| label.trim().to_ascii_uppercase())
        .collect();
    record
        .cells
        .iter()
        .filter(|cell| wanted.contains(&cell.label.to_ascii_uppercase()))
        .map(|cell| cell.label.clone())
        .collect()
}

pub fn compact_row(row: &ImageRow, temp_path: Option<&str>) -> Value {
    let temporary = temp_path.is_some_and(|path| row.folder_path == path);
    let mut value = json!({
        "id": row.id,
        "folderId": row.folder_id,
        "folderPath": display_folder(&row.folder_path, temporary),
        "format": row.format,
        "fileExt": row.file_ext,
        "width": row.width,
        "height": row.height,
        "fileSize": row.file_size,
        "score": row.score,
        "tags": row.tags,
        "importedAt": row.imported_at,
        "modifiedAt": row.modified_at,
        "deletedAt": row.deleted_at,
        "thumbWidth": row.thumb_width,
        "thumbHeight": row.thumb_height,
    });
    if temporary && let Value::Object(object) = &mut value {
        object.insert("temporary".to_owned(), Value::Bool(true));
    }
    value
}

/// `.artbank-temp` 这类内部目录名不该出现在给 AI 的结果里。
pub(crate) fn display_folder(path: &str, temporary: bool) -> String {
    if temporary {
        "临时".to_owned()
    } else {
        path.to_owned()
    }
}

fn temp_folder_path(context: &ReadContext) -> AppResult<Option<String>> {
    context.database.with_reader(crate::library::temp::path)
}

fn dedupe_ids(ids: &[i64]) -> AppResult<Vec<i64>> {
    if ids.is_empty() {
        return Err(AppError::message("ids 不能为空"));
    }
    selection::dedupe_ids(ids, MAX_IMAGE_IDS)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
    use rusqlite::params;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        ai::runtime::AiRuntime,
        app::settings::Settings,
        library::service,
        mcp::{
            context::ReadContext,
            selection::{McpFilters, QueryScope, QueryView},
            sheet::SheetCache,
        },
    };

    struct Fixture {
        _temp: TempDir,
        context: ReadContext,
        ai: AiRuntime,
        works_folder: i64,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "MCP 测试", 1).unwrap();
        let works_folder = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO folders(path, created_at) VALUES ('作品', 1)",
                    [],
                )?;
                let works = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO folders(path, created_at) VALUES ('作品/速写', 2)",
                    [],
                )?;
                let sketches = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO collections(name, sort_order, created_at, updated_at)
                     VALUES ('精选', 0, 1, 1)",
                    [],
                )?;
                connection.execute("INSERT INTO tags(name) VALUES ('人物')", [])?;
                let people = connection.last_insert_rowid();
                connection.execute("INSERT INTO tags(name) VALUES ('风景')", [])?;
                let scenery = connection.last_insert_rowid();
                connection.execute("INSERT INTO tags(name) VALUES ('未使用')", [])?;

                for (id, folder, score, description, imported_at) in [
                    (1_i64, works, 5_i64, "红衣人物", 100_i64),
                    (2, works, 3, "蓝衣人物", 200),
                    (3, sketches, 1, "风景速写", 300),
                ] {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', ?3, ?4, ?5, ?6, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), ?7, ?8, ?9, ?9)",
                        params![
                            id,
                            format!("{id:0>64}"),
                            folder,
                            1000 + id,
                            100 + id,
                            80 + id,
                            score,
                            description,
                            imported_at
                        ],
                    )?;
                }
                connection.execute(
                    "INSERT INTO image_tags(image_id, tag_id) VALUES (1, ?1), (2, ?2)",
                    params![people, scenery],
                )?;
                Ok(works)
            })
            .unwrap();

        for (id, color) in [(1, [220, 60, 60]), (2, [60, 60, 220])] {
            let mut image = RgbImage::new(32, 24);
            for pixel in image.pixels_mut() {
                *pixel = Rgb(color);
            }
            let mut bytes = std::io::Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(image)
                .write_to(&mut bytes, ImageFormat::Png)
                .unwrap();
            let location = session.thumbs.append(bytes.get_ref()).unwrap();
            session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "UPDATE images SET thumb_segment = ?1, thumb_offset = ?2,
                            thumb_length = ?3, thumb_format = 'png', thumb_width = 32,
                            thumb_height = 24, thumb_version = 1 WHERE id = ?4",
                        params![
                            location.segment,
                            location.offset as i64,
                            location.length,
                            id
                        ],
                    )?;
                    Ok(())
                })
                .unwrap();
        }

        let ai = AiRuntime::new();
        ai.library_opened(
            session.info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            root.clone(),
        );
        let context = ReadContext {
            database: session.db,
            thumbs: session.thumbs,
            root,
            info: session.info,
            settings: Settings::default(),
            sheets: Arc::new(parking_lot::Mutex::new(SheetCache::default())),
            resources: Arc::new(crate::concurrency::resources::ResourceTable::new()),
            images: Arc::new(crate::concurrency::budget::ImageBudget::new(2)),
        };
        Fixture {
            _temp: temp,
            context,
            ai,
            works_folder,
        }
    }

    fn ids_query(ids: Vec<i64>) -> Selector {
        Selector::Ids {
            ids,
            exclude_ids: Vec::new(),
        }
    }

    #[test]
    fn search_returns_total_and_next_page() {
        let fixture = fixture();
        let input = SearchImagesInput {
            query: McpQuery {
                filters: McpFilters {
                    folder_ids: vec![fixture.works_folder],
                    ..Default::default()
                },
                ..Default::default()
            },
            page_size: Some(2),
            ..Default::default()
        };
        let value = search_images(&fixture.context, &input).unwrap();
        assert_eq!(value["total"], 3);
        assert_eq!(value["images"].as_array().unwrap().len(), 2);
        assert!(value["nextCursor"].is_object() || value["nextPageToken"].is_string());

        let error = search_images(
            &fixture.context,
            &SearchImagesInput {
                page_size: Some(MAX_SEARCH_PAGE + 1),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("pageSize"));
    }

    #[test]
    fn folders_and_tags_and_collections_lists() {
        let fixture = fixture();
        let roots = folders_list(
            &fixture.context,
            &FoldersListInput {
                parent_path: None,
                limit: None,
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(roots["total"], 1);
        assert_eq!(roots["folders"][0]["path"], "作品");
        assert_eq!(roots["folders"][0]["totalCount"], 3);

        let children = folders_list(
            &fixture.context,
            &FoldersListInput {
                parent_path: Some("作品".into()),
                limit: None,
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(children["total"], 1);
        assert_eq!(children["folders"][0]["path"], "作品/速写");

        let tags = tags_list(
            &fixture.context,
            &TagsListInput {
                query: Some("人".into()),
                limit: None,
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(tags["tags"][0]["name"], "人物");

        let all_tags = tags_list(
            &fixture.context,
            &TagsListInput {
                query: None,
                limit: None,
                cursor: None,
            },
        )
        .unwrap();
        let names: Vec<&str> = all_tags["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| tag["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"未使用"));
        assert!(names.contains(&"人物"));

        let collections = collections_list(&fixture.context).unwrap();
        assert_eq!(collections["collections"][0]["name"], "精选");
    }

    #[test]
    fn images_get_reports_missing_ids() {
        let fixture = fixture();
        let value = images_get(&fixture.context, &[1, 99, 1]).unwrap();
        assert_eq!(value["images"].as_array().unwrap().len(), 1);
        assert_eq!(value["images"][0]["description"], "红衣人物");
        assert_eq!(value["images"][0]["tags"][0]["name"], "人物");
        assert_eq!(value["missingIds"], json!([99]));
    }

    #[test]
    fn selector_ids_excludes_cells_and_missing_rows() {
        let fixture = fixture();
        let page = selection::resolve_page(
            &Selector::Ids {
                ids: vec![1, 2, 3],
                exclude_ids: vec![2],
            },
            &fixture.context,
            &fixture.ai,
            10,
        )
        .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(
            page.slots
                .iter()
                .map(|slot| slot.row.as_ref().unwrap().id)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );

        let missing =
            selection::resolve_page(&ids_query(vec![1, 999]), &fixture.context, &fixture.ai, 10)
                .unwrap();
        assert_eq!(missing.missing_ids, vec![999]);
    }

    #[test]
    fn write_selection_resolves_all_ids_and_missing() {
        let fixture = fixture();
        let selection = selection::resolve_ids(
            &Selector::Ids {
                ids: vec![3, 1, 1, 99],
                exclude_ids: vec![1],
            },
            &fixture.context,
            &fixture.ai,
        )
        .unwrap();
        assert_eq!(selection.ids, vec![3]);
        assert_eq!(selection.missing_ids, vec![99]);

        let selection = selection::resolve_ids(
            &Selector::Query {
                query: Box::new(McpQuery::default()),
                exclude_ids: Vec::new(),
            },
            &fixture.context,
            &fixture.ai,
        )
        .unwrap();
        assert_eq!(selection.ids.len(), 3);
        assert!(selection.missing_ids.is_empty());
    }

    #[test]
    fn image_sheet_returns_mapping_and_cached_cells() {
        let fixture = fixture();
        let input = ImageSheetInput {
            selector: ids_query(vec![1, 2, 3]),
            grid: Some(2),
            resolution: None,
        };
        let result = image_sheet(&fixture.context, &fixture.ai, &input).unwrap();
        let cells = result.mapping["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[0]["label"], "A1");
        assert_eq!(cells[0]["imageId"], 1);
        assert_eq!(cells[0]["status"], "image");
        assert_eq!(cells[2]["label"], "A2");
        assert_eq!(cells[3]["imageId"], Value::Null);
        // 没有图的格子是 empty，不是“有图但读不出缩略图”。
        assert_eq!(cells[3]["status"], "empty");
        assert_eq!(cells[3]["thumbMissing"], false);
        assert!(!result.image.is_empty());
        let sheet_id = result.mapping["sheetId"].as_str().unwrap().to_owned();

        let cached = image_sheet(
            &fixture.context,
            &fixture.ai,
            &ImageSheetInput {
                selector: Selector::Sheet {
                    sheet_id,
                    cells: vec!["B1".into()],
                    exclude_cells: Vec::new(),
                    exclude_ids: Vec::new(),
                },
                grid: Some(1),
                resolution: None,
            },
        )
        .unwrap();
        // 输出按新拼图编号，来源格子 B1 只出现在 sourceLabel。
        assert_eq!(cached.mapping["cells"][0]["label"], "A1");
        assert_eq!(cached.mapping["cells"][0]["sourceLabel"], "B1");
        assert_eq!(cached.mapping["cells"][0]["imageId"], 2);
    }

    #[test]
    fn query_scope_and_facets_follow_library_semantics() {
        let fixture = fixture();
        let value = search_images(
            &fixture.context,
            &SearchImagesInput {
                query: McpQuery {
                    view: QueryView::All,
                    search: "人物".into(),
                    search_scope: QueryScope::Library,
                    filters: McpFilters {
                        folder_ids: vec![fixture.works_folder],
                        ..Default::default()
                    },
                    ..Default::default()
                },
                include_facets: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(value["total"], 2);
        assert_eq!(value["facets"]["total"], 2);
    }

    #[test]
    fn find_similar_preview_and_token_invalidation() {
        let fixture = fixture();
        let first = find_similar(
            &fixture.context,
            &fixture.ai,
            &FindSimilarInput {
                mode: SimilarMode::Image,
                target_id: Some(1),
                kind: Some("phash".into()),
                threshold: Some(6.0),
                limit: Some(10),
                preview_limit: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first["preview"][0]["id"], 2);
        // total 是截断前的全部命中；returned 才是可翻页条数。
        assert_eq!(first["total"], 2);
        assert_eq!(first["returned"], 2);
        assert_eq!(first["scoreKind"], "distance");
        assert_eq!(first["preview"][0]["score"], 0.0);
        let first_token = first["token"].as_str().unwrap().to_owned();

        find_similar(
            &fixture.context,
            &fixture.ai,
            &FindSimilarInput {
                mode: SimilarMode::Image,
                target_id: Some(2),
                kind: Some("phash".into()),
                threshold: Some(6.0),
                limit: Some(10),
                preview_limit: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            fixture
                .ai
                .similarity_page(&first_token, 0, Some(5))
                .is_err(),
            "新的相似搜索应让旧 token 失效"
        );

        let limited = find_similar(
            &fixture.context,
            &fixture.ai,
            &FindSimilarInput {
                mode: SimilarMode::Image,
                target_id: Some(1),
                kind: Some("phash".into()),
                threshold: Some(6.0),
                limit: Some(1),
                preview_limit: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(limited["total"], 2);
        assert_eq!(limited["returned"], 1);
        assert_eq!(limited["limit"], 1);
    }

    #[test]
    fn find_similar_narrows_scope_and_rejects_average_for_phash() {
        let fixture = fixture();
        let scoped = find_similar(
            &fixture.context,
            &fixture.ai,
            &FindSimilarInput {
                mode: SimilarMode::Image,
                target_id: Some(1),
                kind: Some("phash".into()),
                threshold: Some(6.0),
                limit: Some(10),
                preview_limit: Some(5),
                scope: Some(FindSimilarScope::Selection { image_ids: vec![3] }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(scoped["scope"], "选中 1 张");
        assert_eq!(scoped["total"], 1);
        assert_eq!(scoped["preview"][0]["id"], 3);

        assert!(
            find_similar(
                &fixture.context,
                &fixture.ai,
                &FindSimilarInput {
                    mode: SimilarMode::Image,
                    target_ids: Some(vec![1, 2]),
                    kind: Some("phash".into()),
                    threshold: Some(6.0),
                    limit: Some(10),
                    ..Default::default()
                },
            )
            .is_err()
        );

        let averaged = find_similar(
            &fixture.context,
            &fixture.ai,
            &FindSimilarInput {
                mode: SimilarMode::Image,
                target_ids: Some(vec![1, 2]),
                kind: Some("color8".into()),
                threshold: Some(1.0),
                limit: Some(10),
                preview_limit: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(averaged["detail"]["referenceCount"], 2);
        assert_eq!(averaged["total"], 1);
        assert_eq!(averaged["preview"][0]["id"], 3);
    }

    #[test]
    fn similarity_selector_pages_without_repeating_items() {
        let fixture = fixture();
        fixture
            .context
            .database
            .with_writer(|connection| {
                for id in [4_i64, 5] {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', ?3, 1, 10, 10, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 500, 500)",
                        params![id, format!("{id:0>64}"), fixture.works_folder],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        fixture.ai.invalidate_similarity();

        let found = find_similar(
            &fixture.context,
            &fixture.ai,
            &FindSimilarInput {
                mode: SimilarMode::Image,
                target_id: Some(1),
                kind: Some("phash".into()),
                threshold: Some(6.0),
                limit: Some(10),
                preview_limit: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        let token = found["token"].as_str().unwrap().to_owned();

        let mut selector = Selector::Similarity {
            token,
            offset: 0,
            limit: Some(1),
            exclude_ids: Vec::new(),
        };
        let mut seen = Vec::new();
        loop {
            let page =
                selection::resolve_page(&selector, &fixture.context, &fixture.ai, 1).unwrap();
            seen.push(page.slots[0].row.as_ref().unwrap().id);
            match page.next {
                Some(next) => selector = next,
                None => break,
            }
        }
        assert_eq!(seen, vec![2, 3, 4, 5]);
    }

    #[test]
    fn similarity_selector_honours_offset_and_limit_for_writes() {
        let fixture = fixture();
        fixture
            .context
            .database
            .with_writer(|connection| {
                for id in [4_i64, 5] {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', ?3, 1, 10, 10, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 500, 500)",
                        params![id, format!("{id:0>64}"), fixture.works_folder],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        fixture.ai.invalidate_similarity();

        let found = find_similar(
            &fixture.context,
            &fixture.ai,
            &FindSimilarInput {
                mode: SimilarMode::Image,
                target_id: Some(1),
                kind: Some("phash".into()),
                threshold: Some(6.0),
                limit: Some(10),
                preview_limit: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        let token = found["token"].as_str().unwrap().to_owned();

        // 写入也按 offset/limit 切片，不会把整个结果槽都放进来。
        let limited = selection::resolve_ids(
            &Selector::Similarity {
                token: token.clone(),
                offset: 0,
                limit: Some(2),
                exclude_ids: Vec::new(),
            },
            &fixture.context,
            &fixture.ai,
        )
        .unwrap();
        assert_eq!(limited.ids, vec![2, 3]);

        let skipped = selection::resolve_ids(
            &Selector::Similarity {
                token: token.clone(),
                offset: 2,
                limit: Some(2),
                exclude_ids: Vec::new(),
            },
            &fixture.context,
            &fixture.ai,
        )
        .unwrap();
        assert_eq!(skipped.ids, vec![4, 5]);

        // 省略 limit 时仍是“取整个结果槽”。
        let all = selection::resolve_ids(
            &Selector::Similarity {
                token,
                offset: 0,
                limit: None,
                exclude_ids: Vec::new(),
            },
            &fixture.context,
            &fixture.ai,
        )
        .unwrap();
        assert_eq!(all.ids, vec![2, 3, 4, 5]);
    }

    #[test]
    fn library_info_reports_mode_and_limits() {
        let fixture = fixture();
        let value = library_info(&fixture.context, 24).unwrap();
        assert_eq!(value["counts"]["images"], 3);
        assert_eq!(value["mcp"]["mode"], "ask");
        assert_eq!(value["mcp"]["writeTools"], true);
        assert_eq!(value["mcp"]["writeAllowed"], true);
        assert_eq!(value["mcp"]["approval"]["requiredInMode"], "ask");
        assert_eq!(value["mcp"]["approval"]["appliesToCurrentMode"], true);
        assert_eq!(value["mcp"]["approval"]["threshold"], 100);
        assert_eq!(value["mcp"]["approval"]["queueClearedOnFull"], true);
        assert_eq!(value["mcp"]["limits"]["maxSheetCells"], 25);
        assert_eq!(value["mcp"]["limits"]["maxReferenceImages"], 100);
        assert_eq!(value["mcp"]["limits"]["maxImportPaths"], 10_000);
        assert_eq!(value["mcp"]["limits"]["importCompresses"], false);
    }

    #[test]
    fn library_info_hides_threshold_outside_ask_mode() {
        let mut fixture = fixture();
        for (mode, write_allowed, applies, threshold) in [
            (McpMode::Full, true, false, Value::Null),
            (McpMode::ReadOnly, false, false, Value::Null),
        ] {
            fixture.context.settings.mcp.mode = mode;
            let value = library_info(&fixture.context, 24).unwrap();
            assert_eq!(value["mcp"]["writeAllowed"], write_allowed);
            assert_eq!(value["mcp"]["approval"]["appliesToCurrentMode"], applies);
            assert_eq!(value["mcp"]["approval"]["threshold"], threshold);
            assert_eq!(value["mcp"]["writeTools"], true);
        }
    }

    #[test]
    fn tags_list_pages_through_every_tag() {
        let fixture = fixture();
        let mut cursor: Option<String> = None;
        let mut seen = Vec::new();
        loop {
            let page = tags_list(
                &fixture.context,
                &TagsListInput {
                    query: None,
                    limit: Some(1),
                    cursor: cursor.clone(),
                },
            )
            .unwrap();
            assert_eq!(page["total"], 2);
            for tag in page["tags"].as_array().unwrap() {
                seen.push(tag["id"].as_i64().unwrap());
            }
            match page["nextCursor"].as_str() {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 2);

        let error = tags_list(
            &fixture.context,
            &TagsListInput {
                query: None,
                limit: Some(1),
                cursor: Some("第二页".into()),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("游标无效"));
    }
}

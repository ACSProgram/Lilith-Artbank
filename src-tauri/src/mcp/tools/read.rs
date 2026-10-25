use std::collections::HashMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ai::runtime::AiRuntime,
    app::error::{AppError, AppResult},
    imageops::contact_sheet,
    library::{
        capability, collections, metadata,
        paths::{normalize_folder, parent_path},
        query::{self, ImageRow},
    },
};

use crate::mcp::{
    context::ReadContext,
    selection::{self, McpCursor, McpQuery, Selector},
    sheet::{self, SHEET_TTL_MINUTES, SheetResolution},
};

pub const MAX_SEARCH_PAGE: u32 = 50;
pub const MAX_IMAGE_IDS: usize = 50;
pub const MAX_PREVIEW: usize = 25;

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
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TagsListInput {
    /// 按名称模糊匹配；省略时按图片数排序列出。
    pub query: Option<String>,
    #[schemars(description = "最多返回数量，1-500；默认 100。按名称搜索时最多 200")]
    pub limit: Option<u32>,
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

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct FindSimilarInput {
    #[schemars(description = "image 用目标图找相似；text 用语义搜索")]
    pub mode: SimilarMode,
    #[schemars(description = "mode=image 时必填")]
    pub target_id: Option<i64>,
    #[schemars(description = "mode=text 时必填")]
    pub text: Option<String>,
    #[schemars(description = "phash / color4 / color8 / embedding / pose；默认 phash")]
    pub kind: Option<String>,
    #[schemars(description = "pHash 阈值 0-256；默认 6")]
    pub threshold: Option<u32>,
    #[schemars(description = "最多返回结果数，1-10000")]
    pub limit: Option<u32>,
    #[schemars(description = "返回多少条预览，1-25；默认 12")]
    pub preview_limit: Option<u32>,
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
    let (image_count, trash_count) = context.database.counts()?;
    let features = capability::feature_summary(&context.database)?;
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
        },
        "features": {
            "descriptions": features.description,
            "embeddings": features.embedding,
            "poses": features.pose,
            "phash": features.phash,
            "colors": features.colors,
        },
        "ai": {
            "baseUrl": context.settings.ai.base_url,
            "autoStart": context.settings.ai.auto_start,
            "launchConfigured": context.settings.ai.launch_configured(),
        },
        "mcp": {
            "mode": "readOnly",
            "configuredMode": context.settings.mcp.mode.as_str(),
            "toolCount": tool_count,
            "writeTools": false,
            "limits": {
                "maxSearchPage": MAX_SEARCH_PAGE,
                "maxSheetCells": u32::from(contact_sheet::MAX_GRID).pow(2),
                "maxImageIdsPerGet": MAX_IMAGE_IDS,
                "sheetTtlMinutes": SHEET_TTL_MINUTES,
            }
        }
    }))
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
    let limit = input.limit.unwrap_or(200).clamp(1, 500) as usize;
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
    let items: Vec<Value> = children
        .into_iter()
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
    }))
}

pub fn tags_list(context: &ReadContext, input: &TagsListInput) -> AppResult<Value> {
    let query_text = input.query.as_deref().unwrap_or("").trim();
    let limit = input.limit.unwrap_or(100).clamp(1, 500);
    let tags = if query_text.is_empty() {
        query::list_tags(&context.database, Some(limit))?
    } else {
        // search_tags 自己把上限收到 200。
        query::search_tags(&context.database, query_text, Some(limit))?
    };
    Ok(json!({
        "query": query_text,
        "tags": tags.iter().map(|tag| json!({
            "id": tag.id,
            "name": tag.name,
            "imageCount": tag.image_count,
        })).collect::<Vec<_>>(),
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
    let page_size = input
        .page_size
        .unwrap_or(MAX_SEARCH_PAGE)
        .clamp(1, MAX_SEARCH_PAGE);
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
    let mut value = json!({
        "total": total,
        "pageSize": page_size,
        "images": page.items.iter().map(compact_row).collect::<Vec<_>>(),
        "nextCursor": page.next_cursor,
        "nextPageToken": page.next_page_token,
    });
    if let Some(facets) = facets {
        value["facets"] = json!(facets);
    }
    Ok(value)
}

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
    let (token, kind, total, elapsed_ms, extra) = match input.mode {
        SimilarMode::Image => {
            let target_id = input
                .target_id
                .ok_or_else(|| AppError::message("mode=image 时必须提供 targetId"))?;
            let kind = input.kind.as_deref().unwrap_or("phash").trim();
            if !matches!(kind, "phash" | "color4" | "color8" | "embedding" | "pose") {
                return Err(AppError::message(format!(
                    "相似度类型只能是 phash、color4、color8、embedding 或 pose，收到 {kind}"
                )));
            }
            let result = ai.find_similar_images(kind, target_id, input.threshold, Some(limit))?;
            (
                result.token,
                result.kind,
                result.total,
                result.elapsed_ms,
                Value::Null,
            )
        }
        SimilarMode::Text => {
            let text = input
                .text
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| AppError::message("mode=text 时必须提供 text"))?;
            let result = ai.semantic_search(&context.settings.ai, text, Some(limit))?;
            (
                result.token,
                "semantic".to_owned(),
                result.total,
                result.elapsed_ms,
                json!({
                    "model": result.model,
                    "dim": result.dim,
                    "meanCount": result.mean_count,
                }),
            )
        }
    };
    let preview = ai.similarity_page(&token, 0, Some(preview_limit))?;
    Ok(json!({
        "token": token,
        "mode": match input.mode {
            SimilarMode::Image => "image",
            SimilarMode::Text => "text",
        },
        "kind": kind,
        "total": total,
        "elapsedMs": elapsed_ms,
        "preview": preview.items.iter().map(compact_row).collect::<Vec<_>>(),
        "nextOffset": preview.next_offset,
        "detail": extra,
        "note": "相似结果只有一个结果槽；下一次 find_similar 会让这个 token 失效。",
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
    let mut images = Vec::with_capacity(existing.len());
    let mut missing_ids = Vec::new();
    for id in &ids {
        let Some(flags) = capabilities.get(id).copied() else {
            missing_ids.push(*id);
            continue;
        };
        let details = metadata::image_details(&context.database, &context.root, *id)?;
        images.push(json!({
            "id": details.id,
            "contentHash": details.content_hash,
            "filePath": details.file_path,
            "folderId": details.folder_id,
            "folderPath": details.folder_path,
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

    let labels = sheet::labels(grid);
    let mut slots = Vec::with_capacity(limit);
    for (index, label) in labels.into_iter().enumerate() {
        slots.push(match selection.slots.get(index) {
            Some(slot) => sheet::SheetSlot {
                label: slot.label.clone().unwrap_or(label),
                row: slot.row.clone(),
            },
            None => sheet::SheetSlot { label, row: None },
        });
    }
    let built = sheet::build(context, grid, resolution, slots)?;
    let mapping = json!({
        "sheetId": built.record.id,
        "grid": grid,
        "resolution": resolution.as_str(),
        "total": selection.total,
        "next": selection.next,
        "missingIds": selection.missing_ids,
        "cells": built.cells,
    });
    Ok(SheetToolResult {
        mapping,
        image: built.image,
    })
}

pub fn compact_row(row: &ImageRow) -> Value {
    json!({
        "id": row.id,
        "folderId": row.folder_id,
        "folderPath": row.folder_path,
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
    })
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
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "MCP 测试", 1).unwrap();
        session
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
                Ok(())
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
        };
        Fixture {
            _temp: temp,
            context,
            ai,
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
                    folder_ids: vec![2],
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
    }

    #[test]
    fn folders_and_tags_and_collections_lists() {
        let fixture = fixture();
        let roots = folders_list(
            &fixture.context,
            &FoldersListInput {
                parent_path: None,
                limit: None,
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
            },
        )
        .unwrap();
        assert_eq!(tags["tags"][0]["name"], "人物");

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
        assert_eq!(cells[2]["label"], "A2");
        assert_eq!(cells[3]["imageId"], Value::Null);
        assert!(!result.image.is_empty());
        let sheet_id = result.mapping["sheetId"].as_str().unwrap().to_owned();

        let cached = image_sheet(
            &fixture.context,
            &fixture.ai,
            &ImageSheetInput {
                selector: Selector::Sheet {
                    sheet_id,
                    cells: vec!["A1".into()],
                    exclude_cells: Vec::new(),
                    exclude_ids: Vec::new(),
                },
                grid: Some(1),
                resolution: None,
            },
        )
        .unwrap();
        assert_eq!(cached.mapping["cells"][0]["imageId"], 1);
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
                        folder_ids: vec![2],
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
                threshold: Some(6),
                limit: Some(10),
                preview_limit: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first["preview"][0]["id"], 2);
        let first_token = first["token"].as_str().unwrap().to_owned();

        find_similar(
            &fixture.context,
            &fixture.ai,
            &FindSimilarInput {
                mode: SimilarMode::Image,
                target_id: Some(2),
                kind: Some("phash".into()),
                threshold: Some(6),
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
                         ) VALUES (?1, ?2, 'png', 2, 1, 10, 10, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 500, 500)",
                        params![id, format!("{id:0>64}")],
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
                threshold: Some(6),
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
    fn library_info_reports_read_only_limits() {
        let fixture = fixture();
        let value = library_info(&fixture.context, 8).unwrap();
        assert_eq!(value["counts"]["images"], 3);
        assert_eq!(value["mcp"]["mode"], "readOnly");
        assert_eq!(value["mcp"]["writeTools"], false);
        assert_eq!(value["mcp"]["limits"]["maxSheetCells"], 25);
    }
}

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, params_from_iter, types::Value};

use crate::app::error::{AppError, AppResult};
use crate::library::db::LibraryDb;

use super::sql::{
    PreparedQuery, SearchMode, attach_tags, folder_scope_paths, image_row_from_row,
    keyset_condition, normalize, order_by, rows_for_ids, sort_expression, uses_id_cursor,
};
use super::{
    CachedIdList, ID_PAGE_CACHE, ImagePage, ImageRow, LibraryQuery, MAX_CACHED_LISTS, MAX_IDS,
    QueryCursor,
};

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

pub(super) fn parse_page_token(token: &str) -> AppResult<(&str, usize)> {
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

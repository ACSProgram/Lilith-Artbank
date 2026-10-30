use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params_from_iter};

use crate::app::error::{AppError, AppResult};
use crate::library::{db::LibraryDb, folder_tree::FolderTree, placeholders};

use super::sql::{
    Facet, PreparedQuery, SearchMode, count_rows, count_sql, dedupe_ids, filters_only,
    folder_scope_paths, grouped_i64, grouped_sql, grouped_values, normalize, order_by,
    parse_count_key, push_count_branch, sum_rows, sum_sql, tags_grouped_sql,
};
use super::{
    FOLDER_STATS_TAG_LIMIT, FilterCounts, FolderStats, LibraryQuery, LibraryView, MAX_SORT_LEVELS,
    PresenceFilter, SortKey, SortSpec, StatsTag, TagCount, ValueCount,
};

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
pub(super) fn pin_stats_indexes(query: &LibraryQuery) -> bool {
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

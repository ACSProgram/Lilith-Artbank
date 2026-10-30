use std::collections::HashSet;

use rusqlite::{params_from_iter, types::Value};

use crate::app::error::{AppError, AppResult};
use crate::library::db::LibraryDb;

use super::folders::normalize_scope_ids;
use super::sql::{PreparedQuery, SearchMode, folder_scope_paths, normalize};
use super::{LibraryQuery, PendingFilter, PendingStats};

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

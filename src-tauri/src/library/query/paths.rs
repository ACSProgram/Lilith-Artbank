//! 复制路径与内容哈希：按 id 批量取文本，保持传入顺序。

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params_from_iter, types::Value};

use crate::{
    app::error::AppResult,
    library::{db::LibraryDb, paths::LibraryPaths},
};

/// 与其它批量查询一致：SQLite 变量有上限，按 400 个 id 分块。
const SQL_CHUNK: usize = 400;

/// 只取未进释放队列的行：已在释放队列里的图片文件随时会被删，不该出现在剪贴板里。
const PATH_SELECT: &str = "SELECT images.id, images.content_hash, images.file_ext, folders.path
     FROM images
     JOIN folders ON folders.id = images.folder_id
     WHERE images.id IN";

const HASH_SELECT: &str = "SELECT images.id, images.content_hash
     FROM images
     WHERE images.id IN";

/// 按 id 分块查询并收进 `id → T`；`select` 只写到 `IN` 为止，后半段由这里补齐。
fn load_by_ids<T>(
    database: &LibraryDb,
    ids: &[i64],
    select: &str,
    map: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<(i64, T)>,
) -> AppResult<HashMap<i64, T>> {
    database.with_reader(|connection| {
        let mut rows = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(SQL_CHUNK) {
            let sql = format!(
                "{select} ({}) AND images.purge_operation_id IS NULL",
                crate::library::placeholders(chunk.len())
            );
            let values: Vec<Value> = chunk.iter().map(|id| Value::Integer(*id)).collect();
            let mut statement = connection.prepare(&sql)?;
            let loaded = statement
                .query_map(params_from_iter(values.iter()), |row| map(row))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, value) in loaded {
                rows.entry(id).or_insert(value);
            }
        }
        Ok(rows)
    })
}

/// 按传入顺序把查到的行摊成文本，并数出查不到的条数。
fn in_input_order<T>(
    ids: &[i64],
    rows: &HashMap<i64, T>,
    render: impl Fn(&T) -> String,
) -> (Vec<String>, usize) {
    let mut texts = Vec::with_capacity(ids.len());
    let mut missing = 0;
    for id in ids {
        match rows.get(id) {
            Some(value) => texts.push(render(value)),
            None => missing += 1,
        }
    }
    (texts, missing)
}

/// 按输入顺序返回绝对路径；查不到的 id 跳过并计入缺失条数。
///
/// 取 `folders.path` 而不是用户可见的 `original_path`：已删除文件夹的图片实际落在
/// `.artbank/trash/` 下，复制出去的必须是磁盘上真实存在的那个路径。
pub fn paths_for_ids(
    database: &LibraryDb,
    root: &Path,
    ids: &[i64],
) -> AppResult<(Vec<String>, usize)> {
    if ids.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let rows = load_by_ids(database, ids, PATH_SELECT, |row| {
        Ok((
            row.get::<_, i64>(0)?,
            (
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ),
        ))
    })?;

    let paths = LibraryPaths::new(root);
    Ok(in_input_order(ids, &rows, |(hash, extension, folder)| {
        paths
            .content_file(folder, &format!("{hash}.{extension}"))
            .to_string_lossy()
            .into_owned()
    }))
}

/// 按输入顺序返回内容哈希；查不到的 id 跳过并计入缺失条数。
pub fn hashes_for_ids(database: &LibraryDb, ids: &[i64]) -> AppResult<(Vec<String>, usize)> {
    if ids.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let rows = load_by_ids(database, ids, HASH_SELECT, |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(in_input_order(ids, &rows, Clone::clone))
}

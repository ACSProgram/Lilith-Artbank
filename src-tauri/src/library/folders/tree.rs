use rusqlite::Connection;

use crate::{app::error::AppResult, library::folder_tree::FolderTree};

pub(super) fn folder_rows_in(connection: &Connection) -> AppResult<Vec<(i64, String, i64)>> {
    let mut statement = connection.prepare(
        "SELECT id, path, sort_index FROM folders
         WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
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
    Ok(rows)
}

/// 子树内全部文件夹 id（含自身）；空路径表示根，不用于重命名/移动。
pub(super) fn subtree_folder_ids(connection: &Connection, path: &str) -> AppResult<Vec<i64>> {
    Ok(FolderTree::normal(connection)?.ids_under_path(path))
}

/// 从根到指定路径（含）的祖先文件夹 id；用于移动后更新两侧祖先的子树计数。
pub(super) fn ancestor_folder_ids(connection: &Connection, path: &str) -> AppResult<Vec<i64>> {
    Ok(FolderTree::normal(connection)?.ancestors_of_path(path))
}

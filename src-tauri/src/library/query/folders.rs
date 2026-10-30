use std::collections::{HashMap, HashSet};

use crate::app::error::AppResult;
use crate::library::{db::LibraryDb, folder_tree::FolderTree};

use super::{FolderInfo, MAX_IDS, TrashFolders};

/// 每个文件夹的直属图片数。单独提出来是为了让测试能断言查询计划。
pub(super) const FOLDER_COUNTS_SQL: &str = "SELECT folder_id, count(*) FROM images
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

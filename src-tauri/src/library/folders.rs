use std::{collections::HashSet, path::Path};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        history::{
            self, FolderDeleted, FolderPathChange, FolderRef, FolderReorder, FolderRow,
            HistoryKind, NewOperation,
        },
        import::{ensure_folder_tracked, next_sort_index},
        paths::{
            LibraryPaths, join_folder, normalize_folder, normalize_folder_name, parent_path,
            subtree_like_pattern,
        },
        query::{self, FolderInfo},
        service::now_ms,
    },
};

fn folder_id(connection: &Connection, path: &str) -> AppResult<Option<i64>> {
    connection
        .query_row(
            "SELECT id FROM folders WHERE path = ?1 COLLATE NOCASE",
            [path],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::from)
}

fn folder_path(connection: &Connection, id: i64) -> AppResult<String> {
    if id <= 0 {
        return Err(AppError::message("文件夹 id 无效"));
    }
    connection
        .query_row("SELECT path FROM folders WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or_else(|| AppError::message("文件夹不存在"))
}

/// 临时文件夹由生命周期管理，禁止重命名、移动、删除或参与同级排序。
fn ensure_normal_folder(connection: &Connection, id: i64) -> AppResult<()> {
    let kind: Option<String> = connection
        .query_row("SELECT kind FROM folders WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    if kind.as_deref() == Some("temp") {
        return Err(AppError::message("临时文件夹不能重命名、移动、删除或排序"));
    }
    Ok(())
}

fn ensure_inside_root(root: &Path, directory: &Path) -> AppResult<()> {
    let canonical_root = root.canonicalize()?;
    let canonical_directory = directory.canonicalize()?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(AppError::message("文件夹位于图库目录之外"));
    }
    Ok(())
}

pub fn create(db: &LibraryDb, root: &Path, path: &str) -> AppResult<Vec<FolderInfo>> {
    create_with_id(db, root, path).map(|(folders, _)| folders)
}

pub fn create_with_id(
    db: &LibraryDb,
    root: &Path,
    path: &str,
) -> AppResult<(Vec<FolderInfo>, Option<i64>)> {
    let path = normalize_folder(path)?;
    if path.is_empty() {
        return Err(AppError::message("根目录已经存在"));
    }
    let target = LibraryPaths::new(root).folder_dir(&path);
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(HistoryKind::FolderCreate, format!("新建文件夹「{path}」")),
        |transaction, draft| {
            if folder_id(transaction, &path)?.is_some() {
                return Err(AppError::message("文件夹已存在"));
            }
            // 物理目录先落地；ensure_folder 再从根到叶补齐缺失的数据库行。
            std::fs::create_dir_all(&target)?;
            ensure_inside_root(root, &target)?;
            let mut created = Vec::new();
            ensure_folder_tracked(transaction, &path, now_ms(), &mut created)?;
            if created.is_empty() {
                return Err(AppError::message("文件夹已存在"));
            }
            let rows: Vec<FolderRef> = created
                .into_iter()
                .map(|(id, path)| FolderRef { id, path })
                .collect();
            draft.json(&rows)?;
            draft.affected(rows.len() as u64);
            Ok(())
        },
    )?;
    Ok((query::list_folders(db)?, operation_id))
}

pub fn rename(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    name: &str,
) -> AppResult<Vec<FolderInfo>> {
    rename_with_id(db, root, folder_id_value, name).map(|(folders, _)| folders)
}

pub fn rename_with_id(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    name: &str,
) -> AppResult<(Vec<FolderInfo>, Option<i64>)> {
    let name = normalize_folder_name(name)?;
    let paths = LibraryPaths::new(root);
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(
            HistoryKind::FolderRename,
            format!("重命名文件夹为「{name}」"),
        ),
        |transaction, draft| {
            ensure_normal_folder(transaction, folder_id_value)?;
            let old_path = folder_path(transaction, folder_id_value)?;
            if old_path.is_empty() {
                return Err(AppError::message("不能重命名图库根目录"));
            }
            let parent = parent_path(&old_path);
            let new_path = join_folder(parent, &name);
            if old_path == new_path {
                return Ok(());
            }
            if let Some(existing) = folder_id(transaction, &new_path)?
                && existing != folder_id_value
            {
                return Err(AppError::message("目标文件夹已存在"));
            }

            let old_dir = paths.folder_dir(&old_path);
            let new_dir = paths.folder_dir(&new_path);
            if !old_dir.is_dir() {
                return Err(AppError::message(format!(
                    "文件夹目录不存在：{}",
                    old_dir.display()
                )));
            }
            let case_only = new_path.eq_ignore_ascii_case(&old_path);
            if !case_only && new_dir.exists() {
                return Err(AppError::message("目标文件夹目录已存在"));
            }
            rename_directory(&old_dir, &new_dir, case_only)?;
            ensure_inside_root(root, &new_dir)?;

            if let Err(error) =
                update_subtree_paths(transaction, &old_path, &new_path, folder_id_value)
            {
                if let Err(rollback) = rename_directory(&new_dir, &old_dir, true) {
                    log::error!(
                        "回滚文件夹重命名失败：{rollback}；目录位于 {}",
                        new_dir.display()
                    );
                }
                return Err(error);
            }
            let sort_index: i64 = transaction.query_row(
                "SELECT sort_index FROM folders WHERE id = ?1",
                [folder_id_value],
                |row| row.get(0),
            )?;
            draft.json(&FolderPathChange {
                id: folder_id_value,
                old_path,
                new_path,
                old_sort_index: sort_index,
                new_sort_index: sort_index,
            })?;
            draft.affected(1);
            Ok(())
        },
    )?;
    Ok((query::list_folders(db)?, operation_id))
}

pub fn move_to(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    parent_id: i64,
) -> AppResult<Vec<FolderInfo>> {
    move_to_with_id(db, root, folder_id_value, parent_id).map(|(folders, _)| folders)
}

pub fn move_to_with_id(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
    parent_id: i64,
) -> AppResult<(Vec<FolderInfo>, Option<i64>)> {
    let paths = LibraryPaths::new(root);
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(HistoryKind::FolderMove, "移动文件夹"),
        |transaction, draft| {
            ensure_normal_folder(transaction, folder_id_value)?;
            ensure_normal_folder(transaction, parent_id)?;
            let old_path = folder_path(transaction, folder_id_value)?;
            if old_path.is_empty() {
                return Err(AppError::message("不能移动图库根目录"));
            }
            let parent_path = folder_path(transaction, parent_id)?;
            if parent_path == old_path || parent_path.starts_with(&format!("{old_path}/")) {
                return Err(AppError::message("不能把文件夹移动到自身或子文件夹"));
            }
            let leaf = old_path
                .rsplit_once('/')
                .map_or(old_path.as_str(), |(_, leaf)| leaf);
            let new_path = join_folder(&parent_path, leaf);
            if new_path == old_path {
                return Ok(());
            }
            if let Some(existing) = folder_id(transaction, &new_path)?
                && existing != folder_id_value
            {
                return Err(AppError::message("目标位置已经有同名文件夹"));
            }

            let old_dir = paths.folder_dir(&old_path);
            let new_dir = paths.folder_dir(&new_path);
            if !old_dir.is_dir() {
                return Err(AppError::message(format!(
                    "文件夹目录不存在：{}",
                    old_dir.display()
                )));
            }
            if let Some(parent) = new_dir.parent() {
                std::fs::create_dir_all(parent)?;
                ensure_inside_root(root, parent)?;
            }
            if new_dir.exists() {
                return Err(AppError::message("目标文件夹目录已存在"));
            }
            std::fs::rename(&old_dir, &new_dir)?;
            ensure_inside_root(root, &new_dir)?;

            let result = (|| -> AppResult<(i64, i64)> {
                let old_sort_index: i64 = transaction.query_row(
                    "SELECT sort_index FROM folders WHERE id = ?1",
                    [folder_id_value],
                    |row| row.get(0),
                )?;
                let new_sort_index = next_sort_index(transaction, &new_path)?;
                update_subtree_paths(transaction, &old_path, &new_path, folder_id_value)?;
                transaction.execute(
                    "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                    params![new_sort_index, folder_id_value],
                )?;
                Ok((old_sort_index, new_sort_index))
            })();
            let (old_sort_index, new_sort_index) = match result {
                Ok(values) => values,
                Err(error) => {
                    if let Err(rollback) = std::fs::rename(&new_dir, &old_dir) {
                        log::error!(
                            "回滚文件夹移动失败：{rollback}；目录位于 {}",
                            new_dir.display()
                        );
                    }
                    return Err(error);
                }
            };
            draft.json(&FolderPathChange {
                id: folder_id_value,
                old_path,
                new_path,
                old_sort_index,
                new_sort_index,
            })?;
            draft.affected(1);
            Ok(())
        },
    )?;
    Ok((query::list_folders(db)?, operation_id))
}

/// 把同一父级下的若干文件夹移动到指定位置；只调整 `sort_index`，不动目录。
pub fn reorder(
    db: &LibraryDb,
    folder_ids: &[i64],
    parent_id: i64,
    sort_index: i64,
) -> AppResult<Vec<FolderInfo>> {
    if folder_ids.is_empty() {
        return Err(AppError::message("没有要调整顺序的文件夹"));
    }
    let mut moved = HashSet::new();
    for id in folder_ids {
        if *id <= 0 || !moved.insert(*id) {
            return Err(AppError::message("文件夹 id 无效或重复"));
        }
    }
    history::write(
        db,
        NewOperation::new(HistoryKind::FolderReorder, "调整文件夹顺序"),
        |transaction, draft| {
            ensure_normal_folder(transaction, parent_id)?;
            for id in folder_ids {
                ensure_normal_folder(transaction, *id)?;
            }
            let parent = folder_path(transaction, parent_id)?;
            // 同级文件夹一次读出来：手动排序只在拖放时发生，几千行路径的读取代价可以接受。
            let mut statement = transaction.prepare(
                "SELECT id, path, sort_index FROM folders
                 WHERE kind = 'normal' ORDER BY sort_index, id",
            )?;
            let folders = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);

            let siblings: Vec<(i64, String, i64)> = folders
                .into_iter()
                .filter(|(_, path, _)| !path.is_empty() && parent_path(path) == parent)
                .collect();
            if let Some(foreign) = moved
                .iter()
                .find(|id| !siblings.iter().any(|(existing, _, _)| existing == *id))
            {
                return Err(AppError::message(format!(
                    "文件夹 {foreign} 不在目标位置，只能调整同一级文件夹的顺序"
                )));
            }

            let old_positions: Vec<(i64, i64)> = siblings
                .iter()
                .map(|(id, _, index)| (*id, *index))
                .collect();
            let mut order: Vec<i64> = siblings
                .iter()
                .map(|(id, _, _)| *id)
                .filter(|id| !moved.contains(id))
                .collect();
            let insert_at = sort_index.clamp(0, order.len() as i64) as usize;
            order.splice(insert_at..insert_at, folder_ids.iter().copied());

            let changed = order
                .iter()
                .enumerate()
                .filter(|(position, id)| {
                    old_positions
                        .iter()
                        .find(|(existing, _)| existing == *id)
                        .is_some_and(|(_, index)| *index != *position as i64)
                })
                .count();
            if changed == 0 {
                return Ok(());
            }
            for (position, id) in order.iter().enumerate() {
                transaction.execute(
                    "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                    params![position as i64, id],
                )?;
            }
            draft.json(&FolderReorder {
                rows: old_positions,
            })?;
            draft.affected(changed as u64);
            Ok(())
        },
    )?;
    query::list_folders(db)
}

pub fn delete(db: &LibraryDb, root: &Path, folder_id_value: i64) -> AppResult<Vec<FolderInfo>> {
    delete_with_id(db, root, folder_id_value).map(|(folders, _)| folders)
}

pub fn delete_with_id(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
) -> AppResult<(Vec<FolderInfo>, Option<i64>)> {
    let paths = LibraryPaths::new(root);
    let ((), operation_id) = history::write_with_id(
        db,
        NewOperation::new(HistoryKind::FolderDelete, "删除文件夹"),
        |transaction, draft| {
            ensure_normal_folder(transaction, folder_id_value)?;
            let (path, created_at, sort_index): (String, i64, i64) = transaction
                .query_row(
                    "SELECT path, created_at, sort_index FROM folders WHERE id = ?1",
                    [folder_id_value],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| AppError::message("文件夹不存在"))?;
            // 与 MCP 预检共用同一套判据，避免 dryRun 说能删、真删才报错。
            if let Some(reason) = deletion_blocker_in(transaction, root, folder_id_value)? {
                return Err(AppError::message(reason));
            }

            let directory = paths.folder_dir(&path);
            if directory.exists() {
                ensure_inside_root(root, &directory)?;
                if std::fs::read_dir(&directory)?.next().is_some() {
                    return Err(AppError::message("文件夹目录不是空的，不能删除"));
                }
                std::fs::remove_dir(&directory)?;
            }

            let changed =
                transaction.execute("DELETE FROM folders WHERE id = ?1", [folder_id_value])?;
            if changed == 0 {
                if let Err(error) = std::fs::create_dir_all(&directory) {
                    log::error!(
                        "回滚文件夹删除失败：{error}；目录应位于 {}",
                        directory.display()
                    );
                }
                return Err(AppError::message("文件夹不存在"));
            }
            draft.json(&FolderDeleted {
                id: folder_id_value,
                path,
                created_at,
                sort_index,
            })?;
            draft.affected(1);
            Ok(())
        },
    )?;
    Ok((query::list_folders(db)?, operation_id))
}

/// 按相对路径找文件夹 id；大小写不敏感，与建目录时的解析一致。
pub fn find_id(db: &LibraryDb, path: &str) -> AppResult<Option<i64>> {
    let path = normalize_folder(path)?;
    db.with_reader(|connection| folder_id(connection, &path))
}

/// 单个文件夹的位置与图片数；只查这一棵子树，不做全库扫描。
pub fn info(db: &LibraryDb, folder_id_value: i64) -> AppResult<FolderInfo> {
    db.with_reader(|connection| info_in(connection, folder_id_value))
}

fn info_in(connection: &Connection, folder_id_value: i64) -> AppResult<FolderInfo> {
    let (path, sort_index): (String, i64) = connection
        .query_row(
            "SELECT path, sort_index FROM folders WHERE id = ?1",
            [folder_id_value],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::message("文件夹不存在"))?;
    let direct_image_count: i64 = connection.query_row(
        "SELECT count(*) FROM images
         WHERE folder_id = ?1 AND deleted_at IS NULL AND purge_operation_id IS NULL",
        [folder_id_value],
        |row| row.get(0),
    )?;
    let total_image_count: i64 = if path.is_empty() {
        direct_image_count
    } else {
        connection.query_row(
            "SELECT count(*) FROM images JOIN folders ON folders.id = images.folder_id
             WHERE folders.path LIKE ?1 ESCAPE '\\'
               AND images.deleted_at IS NULL AND images.purge_operation_id IS NULL",
            [subtree_like_pattern(&path)],
            |row| row.get(0),
        )?
    };
    Ok(FolderInfo {
        id: folder_id_value,
        path,
        sort_index,
        direct_image_count: direct_image_count.max(0) as u64,
        total_image_count: total_image_count.max(0) as u64,
    })
}

/// 删除的前置条件：没有子文件夹、没有图片、磁盘目录为空。返回不能删的原因。
pub fn deletion_blocker(
    db: &LibraryDb,
    root: &Path,
    folder_id_value: i64,
) -> AppResult<Option<String>> {
    db.with_reader(|connection| deletion_blocker_in(connection, root, folder_id_value))
}

fn deletion_blocker_in(
    connection: &Connection,
    root: &Path,
    folder_id_value: i64,
) -> AppResult<Option<String>> {
    let kind: Option<String> = connection
        .query_row(
            "SELECT kind FROM folders WHERE id = ?1",
            [folder_id_value],
            |row| row.get(0),
        )
        .optional()?;
    if kind.is_none() {
        return Ok(Some("文件夹不存在".to_owned()));
    }
    if kind.as_deref() == Some("temp") {
        return Ok(Some("临时文件夹不能删除".to_owned()));
    }
    let path = folder_path(connection, folder_id_value)?;
    if path.is_empty() {
        return Ok(Some("不能删除图库根目录".to_owned()));
    }
    let children: i64 = connection.query_row(
        "SELECT count(*) FROM folders WHERE path LIKE ?1 ESCAPE '\\'",
        [subtree_like_pattern(&path)],
        |row| row.get(0),
    )?;
    if children > 0 {
        return Ok(Some("文件夹还包含子文件夹，不能删除".to_owned()));
    }
    let images: i64 = connection.query_row(
        "SELECT count(*) FROM images WHERE folder_id = ?1",
        [folder_id_value],
        |row| row.get(0),
    )?;
    if images > 0 {
        return Ok(Some("文件夹中还有图片，不能删除".to_owned()));
    }
    let directory = LibraryPaths::new(root).folder_dir(&path);
    if directory.exists() {
        ensure_inside_root(root, &directory)?;
        if std::fs::read_dir(&directory)?.next().is_some() {
            return Ok(Some("文件夹目录不是空的，不能删除".to_owned()));
        }
    }
    Ok(None)
}

/// 撤销“新建文件夹”：记录里的行仍然存在且为空时删目录与行；已经被移动或已有内容则失败。
pub(crate) fn undo_create(
    transaction: &Transaction<'_>,
    root: &Path,
    created: &[FolderRef],
) -> AppResult<()> {
    let paths = LibraryPaths::new(root);
    let mut rows = created.to_vec();
    rows.sort_by_key(|row| std::cmp::Reverse(row.path.matches('/').count()));
    for item in rows {
        let current: Option<String> = transaction
            .query_row("SELECT path FROM folders WHERE id = ?1", [item.id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(current) = current else {
            continue;
        };
        if !current.eq_ignore_ascii_case(&item.path) {
            return Err(AppError::message("文件夹已被移动，不能撤销新建"));
        }
        remove_folder_if_empty(transaction, &paths, root, item.id, &item.path, true)?;
    }
    Ok(())
}

/// 导入撤销只清理本次新建且仍为空的文件夹；已经有内容的就保留，不让整条记录失败。
pub(crate) fn cleanup_created(
    transaction: &Transaction<'_>,
    root: &Path,
    created: &[FolderRef],
) -> AppResult<()> {
    let paths = LibraryPaths::new(root);
    let mut rows = created.to_vec();
    rows.sort_by_key(|row| std::cmp::Reverse(row.path.matches('/').count()));
    for item in rows {
        let current: Option<String> = transaction
            .query_row("SELECT path FROM folders WHERE id = ?1", [item.id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(current) = current else {
            continue;
        };
        if !current.eq_ignore_ascii_case(&item.path) {
            continue;
        }
        remove_folder_if_empty(transaction, &paths, root, item.id, &item.path, false)?;
    }
    Ok(())
}

fn remove_folder_if_empty(
    transaction: &Transaction<'_>,
    paths: &LibraryPaths,
    root: &Path,
    id: i64,
    path: &str,
    strict: bool,
) -> AppResult<()> {
    let children: i64 = transaction.query_row(
        "SELECT count(*) FROM folders WHERE path LIKE ?1 ESCAPE '\\'",
        [subtree_like_pattern(path)],
        |row| row.get(0),
    )?;
    let images: i64 = transaction.query_row(
        "SELECT count(*) FROM images WHERE folder_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    if children > 0 || images > 0 {
        if strict {
            return Err(AppError::message("文件夹里已经有内容，不能撤销新建"));
        }
        return Ok(());
    }
    let directory = paths.folder_dir(path);
    if directory.exists() {
        ensure_inside_root(root, &directory)?;
        if std::fs::read_dir(&directory)?.next().is_some() {
            if strict {
                return Err(AppError::message("文件夹目录不是空的，不能撤销新建"));
            }
            return Ok(());
        }
        std::fs::remove_dir(&directory)?;
    }
    transaction.execute("DELETE FROM folders WHERE id = ?1", [id])?;
    Ok(())
}

/// 撤销重命名/移动：当前路径是旧路径视为已完成；是目标路径才反向搬回。
pub(crate) fn undo_path_change(
    transaction: &Transaction<'_>,
    root: &Path,
    change: &FolderPathChange,
) -> AppResult<()> {
    let current = folder_path(transaction, change.id)?;
    if current.eq_ignore_ascii_case(&change.old_path) {
        transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![change.old_sort_index, change.id],
        )?;
        return Ok(());
    }
    if !current.eq_ignore_ascii_case(&change.new_path) {
        return Err(AppError::message("文件夹当前路径与记录不符，无法撤销"));
    }

    let paths = LibraryPaths::new(root);
    let old_dir = paths.folder_dir(&change.old_path);
    let new_dir = paths.folder_dir(&change.new_path);
    let case_only = change.old_path.eq_ignore_ascii_case(&change.new_path);
    // 上次中断可能已经把目录搬回，只剩数据库未更新；这时只补 SQL。
    let physical_done = !new_dir.is_dir() && old_dir.is_dir();
    if !physical_done {
        if !new_dir.is_dir() {
            return Err(AppError::message("被移动的文件夹目录不存在，无法撤销"));
        }
        if !case_only && old_dir.exists() {
            return Err(AppError::message("原位置已经有同名文件夹，无法撤销"));
        }
        rename_directory(&new_dir, &old_dir, case_only)?;
        ensure_inside_root(root, &old_dir)?;
    }

    let result = update_subtree_paths(transaction, &change.new_path, &change.old_path, change.id)
        .and_then(|_| {
            transaction.execute(
                "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                params![change.old_sort_index, change.id],
            )?;
            Ok(())
        });
    if let Err(error) = result {
        if let Err(rollback) = rename_directory(&old_dir, &new_dir, true) {
            log::error!(
                "回滚文件夹撤销失败：{rollback}；目录位于 {}",
                old_dir.display()
            );
        }
        return Err(error);
    }
    Ok(())
}

/// 撤销“删除空文件夹”：用原 id 重建数据库行与物理目录；行已存在视为已完成。
pub(crate) fn undo_delete(
    transaction: &Transaction<'_>,
    root: &Path,
    deleted: &FolderDeleted,
) -> AppResult<()> {
    let existing: Option<String> = transaction
        .query_row(
            "SELECT path FROM folders WHERE id = ?1",
            [deleted.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing.eq_ignore_ascii_case(&deleted.path) {
            return Ok(());
        }
        // folders.id 是普通 INTEGER PRIMARY KEY，删除最大 id 后新建可能复用同一个 id。
        return Err(AppError::message(
            "原文件夹 id 已被其他文件夹占用，无法恢复",
        ));
    }
    if let Some(existing) = folder_id(transaction, &deleted.path)? {
        return Err(AppError::message(format!(
            "路径已被文件夹 {existing} 占用，无法恢复"
        )));
    }
    let directory = LibraryPaths::new(root).folder_dir(&deleted.path);
    if directory.exists() && !directory.is_dir() {
        return Err(AppError::message("原文件夹位置被文件占用，无法恢复"));
    }
    std::fs::create_dir_all(&directory)?;
    ensure_inside_root(root, &directory)?;
    transaction.execute(
        "INSERT INTO folders(id, path, created_at, sort_index) VALUES (?1, ?2, ?3, ?4)",
        params![
            deleted.id,
            deleted.path,
            deleted.created_at,
            deleted.sort_index
        ],
    )?;
    Ok(())
}

/// 撤销同级排序：把每个文件夹恢复成记录里的 `sort_index`。
pub(crate) fn undo_reorder(
    transaction: &Transaction<'_>,
    reorder: &FolderReorder,
) -> AppResult<()> {
    for (id, sort_index) in &reorder.rows {
        let changed = transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![sort_index, id],
        )?;
        if changed == 0 {
            return Err(AppError::message("排序记录中的文件夹不存在"));
        }
    }
    Ok(())
}

/// 重做“新建文件夹”：按原 id、路径与排序位补回行和目录；已存在则跳过。
pub(crate) fn redo_create(
    transaction: &Transaction<'_>,
    root: &Path,
    rows: &[FolderRow],
) -> AppResult<()> {
    let paths = LibraryPaths::new(root);
    let mut rows = rows.to_vec();
    rows.sort_by_key(|row| row.path.matches('/').count());
    for item in rows {
        let existing: Option<String> = transaction
            .query_row("SELECT path FROM folders WHERE id = ?1", [item.id], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(existing) = existing {
            if !existing.eq_ignore_ascii_case(&item.path) {
                return Err(AppError::message(format!(
                    "重做时文件夹 id {} 已被其他文件夹占用",
                    item.id
                )));
            }
            let directory = paths.folder_dir(&item.path);
            if !directory.is_dir() {
                std::fs::create_dir_all(&directory)?;
                ensure_inside_root(root, &directory)?;
            }
            continue;
        }
        if let Some(existing) = folder_id(transaction, &item.path)? {
            return Err(AppError::message(format!(
                "重做时路径「{}」已被文件夹 {existing} 占用",
                item.path
            )));
        }
        let directory = paths.folder_dir(&item.path);
        std::fs::create_dir_all(&directory)?;
        ensure_inside_root(root, &directory)?;
        transaction.execute(
            "INSERT INTO folders(id, path, created_at, sort_index) VALUES (?1, ?2, ?3, ?4)",
            params![item.id, item.path, item.created_at, item.sort_index],
        )?;
    }
    Ok(())
}

/// 重做“重命名/移动文件夹”：与撤销对称，当前已在目标路径时只补排序位。
pub(crate) fn redo_path_change(
    transaction: &Transaction<'_>,
    root: &Path,
    change: &FolderPathChange,
) -> AppResult<()> {
    let current = folder_path(transaction, change.id)?;
    if current.eq_ignore_ascii_case(&change.new_path) {
        transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![change.new_sort_index, change.id],
        )?;
        return Ok(());
    }
    if !current.eq_ignore_ascii_case(&change.old_path) {
        return Err(AppError::message("文件夹当前路径与记录不符，无法重做"));
    }

    let paths = LibraryPaths::new(root);
    let old_dir = paths.folder_dir(&change.old_path);
    let new_dir = paths.folder_dir(&change.new_path);
    let case_only = change.old_path.eq_ignore_ascii_case(&change.new_path);
    let physical_done = !old_dir.is_dir() && new_dir.is_dir();
    if !physical_done {
        if !old_dir.is_dir() {
            return Err(AppError::message("文件夹目录不存在，无法重做"));
        }
        if !case_only && new_dir.exists() {
            return Err(AppError::message("目标位置已经有同名文件夹，无法重做"));
        }
        rename_directory(&old_dir, &new_dir, case_only)?;
        ensure_inside_root(root, &new_dir)?;
    }

    let result = update_subtree_paths(transaction, &change.old_path, &change.new_path, change.id)
        .and_then(|_| {
            transaction.execute(
                "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                params![change.new_sort_index, change.id],
            )?;
            Ok(())
        });
    if let Err(error) = result {
        if let Err(rollback) = rename_directory(&new_dir, &old_dir, true) {
            log::error!(
                "回滚文件夹重做失败：{rollback}；目录位于 {}",
                new_dir.display()
            );
        }
        return Err(error);
    }
    Ok(())
}

/// 重做“删除空文件夹”：仍为空时删目录与行；已经不存在视为完成。
pub(crate) fn redo_delete(
    transaction: &Transaction<'_>,
    root: &Path,
    deleted: &FolderDeleted,
) -> AppResult<()> {
    let existing: Option<String> = transaction
        .query_row(
            "SELECT path FROM folders WHERE id = ?1",
            [deleted.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(existing) = existing else {
        return Ok(());
    };
    if !existing.eq_ignore_ascii_case(&deleted.path) {
        return Err(AppError::message(
            "文件夹 id 已被其他文件夹占用，无法重做删除",
        ));
    }
    let pattern = subtree_like_pattern(&deleted.path);
    let children: i64 = transaction.query_row(
        "SELECT count(*) FROM folders WHERE path LIKE ?1 ESCAPE '\\'",
        [pattern],
        |row| row.get(0),
    )?;
    let images: i64 = transaction.query_row(
        "SELECT count(*) FROM images WHERE folder_id = ?1",
        [deleted.id],
        |row| row.get(0),
    )?;
    if children > 0 || images > 0 {
        return Err(AppError::message("文件夹里已经有内容，不能重做删除"));
    }
    let directory = LibraryPaths::new(root).folder_dir(&deleted.path);
    if directory.exists() {
        ensure_inside_root(root, &directory)?;
        if std::fs::read_dir(&directory)?.next().is_some() {
            return Err(AppError::message("文件夹目录不是空的，不能重做删除"));
        }
        std::fs::remove_dir(&directory)?;
    }
    transaction.execute("DELETE FROM folders WHERE id = ?1", [deleted.id])?;
    Ok(())
}

/// 重做同级排序：恢复撤销时捕获的目标排序位。
pub(crate) fn redo_reorder(transaction: &Transaction<'_>, rows: &[(i64, i64)]) -> AppResult<()> {
    for (id, sort_index) in rows {
        let changed = transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![sort_index, id],
        )?;
        if changed == 0 {
            return Err(AppError::message("排序记录中的文件夹不存在"));
        }
    }
    Ok(())
}

fn rename_directory(old: &Path, new: &Path, case_only: bool) -> AppResult<()> {
    if !case_only {
        return std::fs::rename(old, new).map_err(AppError::from);
    }
    // Windows 不允许直接把 Foo 改成 foo（目标被视为已存在），用临时名中转。
    let parent = old.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".artbank-rename-{}", uuid::Uuid::new_v4()));
    std::fs::rename(old, &temporary)?;
    match std::fs::rename(&temporary, new) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::rename(&temporary, old);
            Err(error.into())
        }
    }
}

fn update_subtree_paths(
    connection: &Connection,
    old_path: &str,
    new_path: &str,
    folder_id_value: i64,
) -> AppResult<()> {
    let pattern = subtree_like_pattern(old_path);
    connection.execute(
        "UPDATE folders
         SET path = ?1 || substr(path, length(?2) + 1)
         WHERE path LIKE ?3 ESCAPE '\\' AND id <> ?4",
        params![new_path, old_path, pattern, folder_id_value],
    )?;
    connection.execute(
        "UPDATE folders SET path = ?1 WHERE id = ?2",
        params![new_path, folder_id_value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc};

    use tempfile::tempdir;

    use super::*;
    use crate::library::{history, service};

    fn fixture() -> (tempfile::TempDir, Arc<LibraryDb>, PathBuf) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "文件夹测试", 1).unwrap();
        let database = session.db.clone();
        (temp, database, root)
    }

    #[test]
    fn creates_nested_folders_with_ancestors() {
        let (_temp, db, root) = fixture();
        let folders = create(&db, &root, "人物/速写/手部").unwrap();
        let paths: Vec<&str> = folders.iter().map(|item| item.path.as_str()).collect();
        assert!(paths.contains(&"人物"));
        assert!(paths.contains(&"人物/速写"));
        assert!(paths.contains(&"人物/速写/手部"));
        assert!(root.join("人物").join("速写").join("手部").is_dir());
        assert!(create(&db, &root, "人物").is_err());
        assert!(create(&db, &root, "CON").is_err());

        let entries = history::list(&db).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, HistoryKind::FolderCreate);
        assert_eq!(entries[0].affected, 3);
    }

    #[test]
    fn renames_and_moves_subtrees() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "人物/速写").unwrap();
        let people = db
            .with_reader(|connection| folder_id(connection, "人物"))
            .unwrap()
            .unwrap();
        let sketch = db
            .with_reader(|connection| folder_id(connection, "人物/速写"))
            .unwrap()
            .unwrap();
        let scenery = create(&db, &root, "风景").unwrap();
        let scenery_id = scenery
            .iter()
            .find(|item| item.path == "风景")
            .map(|item| item.id)
            .unwrap();

        rename(&db, &root, sketch, "草稿").unwrap();
        assert!(root.join("人物").join("草稿").is_dir());
        assert!(!root.join("人物").join("速写").exists());
        let sketch = db
            .with_reader(|connection| folder_id(connection, "人物/草稿"))
            .unwrap()
            .unwrap();

        move_to(&db, &root, sketch, scenery_id).unwrap();
        assert!(root.join("风景").join("草稿").is_dir());
        let moved = db
            .with_reader(|connection| folder_id(connection, "风景/草稿"))
            .unwrap();
        assert_eq!(moved, Some(sketch));

        assert!(move_to(&db, &root, people, people).is_err());
        assert!(move_to(&db, &root, people, scenery_id).is_ok());
    }

    #[test]
    fn renames_case_only_and_rejects_conflicts() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "Sketches").unwrap();
        let id = db
            .with_reader(|connection| folder_id(connection, "Sketches"))
            .unwrap()
            .unwrap();
        rename(&db, &root, id, "sketches").unwrap();
        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|name| name == "sketches"));
        assert!(!names.iter().any(|name| name == "Sketches"));
        assert!(create(&db, &root, "SKETCHES").is_err());
    }

    #[test]
    fn reorders_siblings_and_rejects_foreign_folders() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "一").unwrap();
        create(&db, &root, "二").unwrap();
        create(&db, &root, "三").unwrap();
        create(&db, &root, "组/子").unwrap();
        let id = |path: &str| {
            db.with_reader(|connection| folder_id(connection, path))
                .unwrap()
                .unwrap()
        };
        let root_id = id("");
        let (one, two, three) = (id("一"), id("二"), id("三"));
        let group_child = id("组/子");

        let order = |db: &LibraryDb| {
            db.with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT path FROM folders
                     WHERE kind = 'normal' AND instr(path, '/') = 0 AND path <> ''
                     ORDER BY sort_index, id",
                )?;
                let paths = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(paths)
            })
            .unwrap()
        };

        reorder(&db, &[three], root_id, 0).unwrap();
        assert_eq!(order(&db), vec!["三", "一", "二", "组"]);

        reorder(&db, &[one], root_id, 10).unwrap();
        reorder(&db, &[two], root_id, 0).unwrap();
        assert_eq!(order(&db), vec!["二", "三", "组", "一"]);

        assert!(reorder(&db, &[group_child], root_id, 0).is_err());
        assert!(reorder(&db, &[one, two], three, 0).is_err());
    }

    #[test]
    fn deletes_only_empty_folders() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "空文件夹").unwrap();
        create(&db, &root, "有子级/子级").unwrap();
        let empty = db
            .with_reader(|connection| folder_id(connection, "空文件夹"))
            .unwrap()
            .unwrap();
        let parent = db
            .with_reader(|connection| folder_id(connection, "有子级"))
            .unwrap()
            .unwrap();

        assert!(delete(&db, &root, parent).is_err());
        delete(&db, &root, empty).unwrap();
        assert!(!root.join("空文件夹").exists());
        assert!(
            db.with_reader(|connection| folder_id(connection, "空文件夹"))
                .unwrap()
                .is_none()
        );

        let untracked = root.join("外部目录");
        fs::create_dir_all(&untracked).unwrap();
        fs::write(untracked.join("keep.txt"), b"x").unwrap();
        create(&db, &root, "外部目录").unwrap();
        let external = db
            .with_reader(|connection| folder_id(connection, "外部目录"))
            .unwrap()
            .unwrap();
        assert!(delete(&db, &root, external).is_err());
        assert!(untracked.join("keep.txt").exists());
    }

    #[test]
    fn undo_restores_created_renamed_moved_deleted_and_reordered_folders() {
        let (_temp, db, root) = fixture();
        create(&db, &root, "人物/速写").unwrap();
        let people = db
            .with_reader(|connection| folder_id(connection, "人物"))
            .unwrap()
            .unwrap();
        let sketch = db
            .with_reader(|connection| folder_id(connection, "人物/速写"))
            .unwrap()
            .unwrap();
        rename(&db, &root, sketch, "草稿").unwrap();
        let scenery = create(&db, &root, "风景").unwrap();
        let scenery_id = scenery
            .iter()
            .find(|item| item.path == "风景")
            .map(|item| item.id)
            .unwrap();
        move_to(&db, &root, sketch, scenery_id).unwrap();
        let root_id = db
            .with_reader(|connection| folder_id(connection, ""))
            .unwrap()
            .unwrap();
        reorder(&db, &[scenery_id, people], root_id, 0).unwrap();
        delete(&db, &root, sketch).unwrap();

        let entries = history::list(&db).unwrap();
        let target = entries.last().unwrap().id;
        let thumbs = crate::library::thumbs::ThumbPack::open(
            crate::library::paths::LibraryPaths::new(&root).thumbs,
        )
        .unwrap();
        let task = crate::app::tasks::TaskHandle::new(
            "folder-undo",
            crate::app::tasks::TaskKind::Undo,
            "测试",
        );
        history::run_undo(&db, &root, &thumbs, target, task).unwrap();

        let entries = history::list(&db).unwrap();
        assert_eq!(entries.len(), 6);
        assert!(
            entries
                .iter()
                .all(|entry| entry.status == crate::library::history::OperationStatus::Undone)
        );
        assert!(!root.join("人物").exists());
        assert!(!root.join("风景").exists());
        assert!(
            db.with_reader(|connection| folder_id(connection, "人物/速写"))
                .unwrap()
                .is_none()
        );

        let target = entries[0].id;
        let task = crate::app::tasks::TaskHandle::new(
            "folder-redo",
            crate::app::tasks::TaskKind::Redo,
            "测试",
        );
        history::run_redo(&db, &root, &thumbs, target, task).unwrap();
        let entries = history::list(&db).unwrap();
        assert!(
            entries
                .iter()
                .all(|entry| entry.status == crate::library::history::OperationStatus::Applied)
        );
        assert!(root.join("人物").is_dir());
        assert!(root.join("风景").is_dir());
        assert!(!root.join("风景").join("草稿").exists());
        assert!(!root.join("人物").join("速写").exists());
        assert!(!root.join("人物").join("草稿").exists());
        let order: Vec<String> = db
            .with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT path FROM folders
                     WHERE kind = 'normal' AND instr(path, '/') = 0 AND path <> ''
                     ORDER BY sort_index, id",
                )?;
                let paths = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(paths)
            })
            .unwrap();
        assert_eq!(order, vec!["风景".to_owned(), "人物".to_owned()]);
    }
}

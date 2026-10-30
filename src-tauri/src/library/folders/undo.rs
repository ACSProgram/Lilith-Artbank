use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        fs_journal::{self, FileJournal},
        history::{
            FolderDeleted, FolderPathChange, FolderRef, FolderReorder, FolderRow, FolderSortChange,
        },
        paths::{LibraryPaths, same_folder, subtree_like_pattern},
        service::now_ms,
    },
};

use super::mutate::remove_folder_if_empty;
use super::trash::{restore_folder_rows, trash_folder_rows};
use super::{ensure_inside_root, folder_id, folder_path};

/// 撤销“新建文件夹”：记录里的行仍然存在且为空时删目录与行；已经被移动或已有内容则失败。
pub(crate) fn undo_create(
    transaction: &Transaction<'_>,
    root: &Path,
    created: &[FolderRef],
    journal: &mut FileJournal,
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
        if !same_folder(&current, &item.path) {
            return Err(AppError::message("文件夹已被移动，不能撤销新建"));
        }
        remove_folder_if_empty(
            transaction,
            &paths,
            root,
            item.id,
            &item.path,
            true,
            journal,
        )?;
    }
    Ok(())
}

/// 导入撤销只清理本次新建且仍为空的文件夹；已经有内容的就保留，不让整条记录失败。
pub(crate) fn cleanup_created(
    transaction: &Transaction<'_>,
    root: &Path,
    created: &[FolderRef],
    journal: &mut FileJournal,
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
        if !same_folder(&current, &item.path) {
            continue;
        }
        remove_folder_if_empty(
            transaction,
            &paths,
            root,
            item.id,
            &item.path,
            false,
            journal,
        )?;
    }
    Ok(())
}

/// 撤销重命名/移动：当前路径是旧路径视为已完成；是目标路径才反向搬回。
pub(crate) fn undo_path_change(
    transaction: &Transaction<'_>,
    root: &Path,
    change: &FolderPathChange,
    journal: &mut FileJournal,
) -> AppResult<()> {
    let current = folder_path(transaction, change.id)?;
    if same_folder(&current, &change.old_path) {
        transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![change.old_sort_index, change.id],
        )?;
        return Ok(());
    }
    if !same_folder(&current, &change.new_path) {
        return Err(AppError::message("文件夹当前路径与记录不符，无法撤销"));
    }

    let paths = LibraryPaths::new(root);
    let old_dir = paths.folder_dir(&change.old_path);
    let new_dir = paths.folder_dir(&change.new_path);
    let case_only = same_folder(&change.old_path, &change.new_path);
    // 上次中断可能已经把目录搬回，只剩数据库未更新；这时只补 SQL。
    let physical_done = !new_dir.is_dir() && old_dir.is_dir();
    if !physical_done {
        if !new_dir.is_dir() {
            return Err(AppError::message("被移动的文件夹目录不存在，无法撤销"));
        }
        if !case_only && old_dir.exists() {
            return Err(AppError::message("原位置已经有同名文件夹，无法撤销"));
        }
        fs_journal::rename_path(&new_dir, &old_dir, case_only)?;
        journal.renamed(&new_dir, &old_dir);
        ensure_inside_root(root, &old_dir)?;
    }

    update_subtree_paths(transaction, &change.new_path, &change.old_path, change.id).and_then(
        |_| {
            transaction.execute(
                "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                params![change.old_sort_index, change.id],
            )?;
            Ok(())
        },
    )
}

/// 撤销“文件夹移入回收站”：文件夹行回到正常树，图片恢复旧状态与修改时间。
pub(crate) fn undo_trash(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    triples: &[(i64, i64, i64)],
) -> AppResult<()> {
    restore_folder_rows(transaction, rows)?;
    for (image_id, deleted_at, modified_at) in triples {
        transaction.execute(
            "UPDATE images SET deleted_at = ?1, modified_at = ?2
             WHERE id = ?3 AND purge_operation_id IS NULL",
            params![
                if *deleted_at == 0 {
                    None
                } else {
                    Some(*deleted_at)
                },
                modified_at,
                image_id
            ],
        )?;
    }
    Ok(())
}

/// 重做“文件夹移入回收站”：文件仍在原路径，只重新写隐藏时间。
pub(crate) fn redo_trash(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    triples: &[(i64, i64, i64)],
) -> AppResult<()> {
    let stamp = now_ms();
    trash_folder_rows(transaction, rows, stamp)?;
    for (image_id, _, _) in triples {
        transaction.execute(
            "UPDATE images SET deleted_at = ?1, modified_at = ?1
             WHERE id = ?2 AND deleted_at IS NULL AND purge_operation_id IS NULL",
            params![stamp, image_id],
        )?;
    }
    Ok(())
}

/// 撤销“恢复文件夹”：重新移入回收站，图片回到恢复前的状态。
pub(crate) fn undo_restore(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    triples: &[(i64, i64, i64)],
) -> AppResult<()> {
    let stamp = triples
        .iter()
        .map(|(_, deleted_at, _)| *deleted_at)
        .max()
        .filter(|value| *value > 0)
        .unwrap_or_else(now_ms);
    trash_folder_rows(transaction, rows, stamp)?;
    for (image_id, deleted_at, modified_at) in triples {
        transaction.execute(
            "UPDATE images SET deleted_at = ?1, modified_at = ?2
             WHERE id = ?3 AND purge_operation_id IS NULL",
            params![
                if *deleted_at == 0 {
                    None
                } else {
                    Some(*deleted_at)
                },
                modified_at,
                image_id
            ],
        )?;
    }
    Ok(())
}

/// 重做“恢复文件夹”：文件夹行与图片重新回到正常树。
pub(crate) fn redo_restore(
    transaction: &Transaction<'_>,
    rows: &[FolderRow],
    triples: &[(i64, i64, i64)],
) -> AppResult<()> {
    let now = now_ms();
    restore_folder_rows(transaction, rows)?;
    for (image_id, _, _) in triples {
        transaction.execute(
            "UPDATE images SET deleted_at = NULL, modified_at = ?1
             WHERE id = ?2 AND purge_operation_id IS NULL",
            params![now, image_id],
        )?;
    }
    Ok(())
}

/// 撤销“删除空文件夹”：用原 id 重建数据库行与物理目录；行已存在视为已完成。
pub(crate) fn undo_delete(
    transaction: &Transaction<'_>,
    root: &Path,
    deleted: &FolderDeleted,
    journal: &mut FileJournal,
) -> AppResult<()> {
    let existing: Option<String> = transaction
        .query_row(
            "SELECT path FROM folders WHERE id = ?1",
            [deleted.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if same_folder(&existing, &deleted.path) {
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
    let created = !directory.exists();
    std::fs::create_dir_all(&directory)?;
    if created {
        journal.created(&directory);
    }
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

/// 撤销批量移动时目标父级兄弟的排序位变化。
pub(crate) fn undo_sort_change(
    transaction: &Transaction<'_>,
    change: &FolderSortChange,
) -> AppResult<()> {
    for (id, old_index, _) in &change.rows {
        let changed = transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![old_index, id],
        )?;
        if changed == 0 {
            return Err(AppError::message("排序记录中的文件夹不存在"));
        }
    }
    Ok(())
}

/// 重做批量移动时目标父级兄弟的排序位变化。
pub(crate) fn redo_sort_change(
    transaction: &Transaction<'_>,
    change: &FolderSortChange,
) -> AppResult<()> {
    for (id, _, new_index) in &change.rows {
        let changed = transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![new_index, id],
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
    journal: &mut FileJournal,
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
            if !same_folder(&existing, &item.path) {
                return Err(AppError::message(format!(
                    "重做时文件夹 id {} 已被其他文件夹占用",
                    item.id
                )));
            }
            let directory = paths.folder_dir(&item.path);
            if !directory.is_dir() {
                std::fs::create_dir_all(&directory)?;
                journal.created(&directory);
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
        journal.created(&directory);
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
    journal: &mut FileJournal,
) -> AppResult<()> {
    let current = folder_path(transaction, change.id)?;
    if same_folder(&current, &change.new_path) {
        transaction.execute(
            "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
            params![change.new_sort_index, change.id],
        )?;
        return Ok(());
    }
    if !same_folder(&current, &change.old_path) {
        return Err(AppError::message("文件夹当前路径与记录不符，无法重做"));
    }

    let paths = LibraryPaths::new(root);
    let old_dir = paths.folder_dir(&change.old_path);
    let new_dir = paths.folder_dir(&change.new_path);
    let case_only = same_folder(&change.old_path, &change.new_path);
    let physical_done = !old_dir.is_dir() && new_dir.is_dir();
    if !physical_done {
        if !old_dir.is_dir() {
            return Err(AppError::message("文件夹目录不存在，无法重做"));
        }
        if !case_only && new_dir.exists() {
            return Err(AppError::message("目标位置已经有同名文件夹，无法重做"));
        }
        fs_journal::rename_path(&old_dir, &new_dir, case_only)?;
        journal.renamed(&old_dir, &new_dir);
        ensure_inside_root(root, &new_dir)?;
    }

    update_subtree_paths(transaction, &change.old_path, &change.new_path, change.id).and_then(
        |_| {
            transaction.execute(
                "UPDATE folders SET sort_index = ?1 WHERE id = ?2",
                params![change.new_sort_index, change.id],
            )?;
            Ok(())
        },
    )
}

/// 重做“删除空文件夹”：仍为空时删目录与行；已经不存在视为完成。
pub(crate) fn redo_delete(
    transaction: &Transaction<'_>,
    root: &Path,
    deleted: &FolderDeleted,
    journal: &mut FileJournal,
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
    if !same_folder(&existing, &deleted.path) {
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
        journal.removed(&directory);
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

pub(super) fn update_subtree_paths(
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

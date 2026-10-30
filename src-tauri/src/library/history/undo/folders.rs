use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::{
    app::error::{AppError, AppResult},
    library::{db::LibraryDb, folders, fs_journal::FileJournal},
};

use super::super::{
    ChunkFormat, FolderDeleted, FolderPathChange, FolderRef, FolderReorder, FolderRow,
    FolderSortChange, HistoryKind, RedoRecord,
    chunks::{decode_json, decode_triples, load_chunks},
    replay::{append_redo, load_redo_records},
};

/// 文件夹记录：所有逆操作在同一个事务里完成；需要重做的完整行在删除前写入 redo chunk。
pub(crate) fn undo_folder(
    database: &LibraryDb,
    root: &Path,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let chunks = load_chunks(&transaction, operation_id)?;
        if matches!(kind, HistoryKind::FolderTrash | HistoryKind::FolderRestore) {
            let mut rows: Vec<FolderRow> = Vec::new();
            let mut triples: Vec<(i64, i64, i64)> = Vec::new();
            for chunk in &chunks {
                match chunk.format {
                    ChunkFormat::Json => rows.extend(decode_json::<Vec<FolderRow>>(&chunk.data)?),
                    ChunkFormat::Triples => triples.extend(decode_triples(&chunk.data)?),
                    _ => {}
                }
            }
            if rows.is_empty() {
                return Err(AppError::message("这条记录缺少重做数据"));
            }
            match kind {
                HistoryKind::FolderTrash => folders::undo_trash(&transaction, &rows, &triples)?,
                HistoryKind::FolderRestore => folders::undo_restore(&transaction, &rows, &triples)?,
                _ => {}
            }
            transaction.commit()?;
            return Ok(());
        }
        // 一次操作可能有多条文件夹路径变更；任何一条失败都算整次失败：
        // 已改过的目录逆序改回去，和随后回滚的数据库事务回到同一状态。
        let mut journal = FileJournal::default();
        let applied = (|| -> AppResult<()> {
            for chunk in chunks {
                if chunk.format == ChunkFormat::SortJson {
                    if matches!(kind, HistoryKind::FolderRename | HistoryKind::FolderMove) {
                        let change: FolderSortChange = decode_json(&chunk.data)?;
                        folders::undo_sort_change(&transaction, &change)?;
                    }
                    continue;
                }
                if chunk.format != ChunkFormat::Json {
                    continue;
                }
                match kind {
                    HistoryKind::FolderCreate => {
                        let rows: Vec<FolderRef> = decode_json(&chunk.data)?;
                        let existing = folder_rows(&transaction, &rows)?;
                        if !existing.is_empty() {
                            append_redo(
                                &transaction,
                                operation_id,
                                &RedoRecord::FolderRows { items: existing },
                            )?;
                        }
                        folders::undo_create(&transaction, root, &rows, &mut journal)?;
                    }
                    HistoryKind::FolderRename | HistoryKind::FolderMove => {
                        let change: FolderPathChange = decode_json(&chunk.data)?;
                        folders::undo_path_change(&transaction, root, &change, &mut journal)?;
                    }
                    HistoryKind::FolderDelete => {
                        let deleted: FolderDeleted = decode_json(&chunk.data)?;
                        folders::undo_delete(&transaction, root, &deleted, &mut journal)?;
                    }
                    HistoryKind::FolderReorder => {
                        let reorder: FolderReorder = decode_json(&chunk.data)?;
                        let mut current = Vec::new();
                        for (id, _) in &reorder.rows {
                            let sort_index: Option<i64> = transaction
                                .query_row(
                                    "SELECT sort_index FROM folders WHERE id = ?1",
                                    [id],
                                    |row| row.get(0),
                                )
                                .optional()?;
                            if let Some(sort_index) = sort_index {
                                current.push((*id, sort_index));
                            }
                        }
                        if !current.is_empty() {
                            append_redo(
                                &transaction,
                                operation_id,
                                &RedoRecord::FolderSort { rows: current },
                            )?;
                        }
                        folders::undo_reorder(&transaction, &reorder)?;
                    }
                    _ => {}
                }
            }
            Ok(())
        })();
        if let Err(error) = applied {
            journal.rollback();
            return Err(error);
        }
        if let Err(error) = transaction.commit() {
            journal.rollback();
            return Err(error.into());
        }
        Ok(())
    })
}

pub(super) fn folder_rows(
    connection: &Connection,
    refs: &[FolderRef],
) -> AppResult<Vec<FolderRow>> {
    let mut rows = Vec::with_capacity(refs.len());
    for item in refs {
        let row: Option<(String, i64, i64)> = connection
            .query_row(
                "SELECT path, created_at, sort_index FROM folders WHERE id = ?1",
                [item.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((path, created_at, sort_index)) = row
            && path.eq_ignore_ascii_case(&item.path)
        {
            rows.push(FolderRow {
                id: item.id,
                path: item.path.clone(),
                created_at,
                sort_index,
            });
        }
    }
    Ok(rows)
}

pub(crate) fn redo_folder(
    database: &LibraryDb,
    root: &Path,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let chunks = load_chunks(&transaction, operation_id)?;
        if matches!(kind, HistoryKind::FolderTrash | HistoryKind::FolderRestore) {
            let mut rows: Vec<FolderRow> = Vec::new();
            let mut triples: Vec<(i64, i64, i64)> = Vec::new();
            for chunk in &chunks {
                match chunk.format {
                    ChunkFormat::Json => rows.extend(decode_json::<Vec<FolderRow>>(&chunk.data)?),
                    ChunkFormat::Triples => triples.extend(decode_triples(&chunk.data)?),
                    _ => {}
                }
            }
            if rows.is_empty() {
                return Err(AppError::message("这条记录缺少重做数据"));
            }
            match kind {
                HistoryKind::FolderTrash => folders::redo_trash(&transaction, &rows, &triples)?,
                HistoryKind::FolderRestore => folders::redo_restore(&transaction, &rows, &triples)?,
                _ => {}
            }
            transaction.commit()?;
            return Ok(());
        }
        // 与撤销对称：任一条失败都逆序撤回已经落地的目录动作。
        let mut journal = FileJournal::default();
        let applied = (|| -> AppResult<()> {
            match kind {
                HistoryKind::FolderCreate => {
                    let mut rows = Vec::new();
                    for record in load_redo_records(&transaction, operation_id)? {
                        if let RedoRecord::FolderRows { items } = record {
                            rows.extend(items);
                        }
                    }
                    if rows.is_empty() {
                        return Err(AppError::message("这条记录缺少重做数据"));
                    }
                    folders::redo_create(&transaction, root, &rows, &mut journal)?;
                }
                HistoryKind::FolderRename | HistoryKind::FolderMove => {
                    for chunk in chunks
                        .iter()
                        .filter(|chunk| chunk.format == ChunkFormat::Json)
                    {
                        let change: FolderPathChange = decode_json(&chunk.data)?;
                        folders::redo_path_change(&transaction, root, &change, &mut journal)?;
                    }
                    for chunk in chunks
                        .iter()
                        .filter(|chunk| chunk.format == ChunkFormat::SortJson)
                    {
                        let change: FolderSortChange = decode_json(&chunk.data)?;
                        folders::redo_sort_change(&transaction, &change)?;
                    }
                }
                HistoryKind::FolderDelete => {
                    let mut found = false;
                    for chunk in chunks
                        .iter()
                        .filter(|chunk| chunk.format == ChunkFormat::Json)
                    {
                        let deleted: FolderDeleted = decode_json(&chunk.data)?;
                        folders::redo_delete(&transaction, root, &deleted, &mut journal)?;
                        found = true;
                    }
                    if !found {
                        return Err(AppError::message("这条记录缺少重做数据"));
                    }
                }
                HistoryKind::FolderReorder => {
                    let mut rows = Vec::new();
                    for record in load_redo_records(&transaction, operation_id)? {
                        if let RedoRecord::FolderSort { rows: items } = record {
                            rows.extend(items);
                        }
                    }
                    if rows.is_empty() {
                        return Err(AppError::message("这条记录缺少重做数据"));
                    }
                    folders::redo_reorder(&transaction, &rows)?;
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = applied {
            journal.rollback();
            return Err(error);
        }
        if let Err(error) = transaction.commit() {
            journal.rollback();
            return Err(error.into());
        }
        Ok(())
    })
}

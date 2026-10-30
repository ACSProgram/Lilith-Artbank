use std::{collections::HashSet, fs, path::Path, sync::Arc};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::TaskHandle,
    },
    library::{
        BatchReporter, db::LibraryDb, metadata, paths::LibraryPaths, service::now_ms,
        thumbs::ThumbPack,
    },
};

use super::chunks::load_chunks;
use super::undo::{
    collections::{redo_collection, undo_collection},
    edit::{redo_edit, undo_edit},
    folders::{redo_folder, undo_folder},
    import::{redo_import, undo_import},
    metadata::{
        redo_description, redo_orphan_move, redo_score, undo_description, undo_orphan_move,
        undo_score,
    },
    r#move::{redo_move, undo_move},
    tags::{redo_tags, undo_tags},
    visibility::{redo_visibility, undo_visibility},
};
use super::*;

pub fn list(database: &LibraryDb) -> AppResult<Vec<HistoryEntry>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, kind, label, created_at, affected, status, error
             FROM operations
             WHERE status IN ('applied', 'undoFailed', 'undoSkipped', 'undone', 'redoing')
             ORDER BY id DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, kind, label, created_at, affected, status, error)| {
                Ok(HistoryEntry {
                    id,
                    kind: HistoryKind::parse(&kind)?,
                    label,
                    created_at,
                    affected: affected.max(0) as u64,
                    status: OperationStatus::parse(&status)?,
                    error,
                })
            })
            .collect()
    })
}

type OperationRow = (i64, String, String, i64, i64, String, String);

fn read_operation_rows(
    database: &LibraryDb,
    sql: &str,
    target_id: i64,
) -> AppResult<Vec<OperationRow>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(sql)?;
        let rows = statement
            .query_map([target_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

fn operation_entry(row: &OperationRow) -> AppResult<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.0,
        kind: HistoryKind::parse(&row.1)?,
        label: row.2.clone(),
        created_at: row.3,
        affected: row.4.max(0) as u64,
        status: OperationStatus::parse(&row.5)?,
        error: row.6.clone(),
    })
}

/// 与 `run_undo` 相同的目标与线性链判定，但不写数据库。
pub fn preview_undo(database: &LibraryDb, target_id: i64) -> AppResult<OperationPreview> {
    let rows = read_operation_rows(
        database,
        "SELECT id, kind, label, created_at, affected, status, error
         FROM operations WHERE id >= ?1 ORDER BY id DESC",
        target_id,
    )?;
    let target = rows
        .iter()
        .find(|row| row.0 == target_id)
        .ok_or_else(|| AppError::message("操作记录不存在"))?;
    match OperationStatus::parse(&target.5)? {
        OperationStatus::Applied => {}
        OperationStatus::UndoFailed => {
            return Err(AppError::message(
                "这条记录上次撤销失败，需要先处理失败原因",
            ));
        }
        OperationStatus::Undone | OperationStatus::Redoing => {
            return Err(AppError::message("这条记录已经撤销，可以重做"));
        }
        OperationStatus::UndoSkipped => {
            return Err(AppError::message("这条记录已跳过撤销"));
        }
        OperationStatus::Pending | OperationStatus::Undoing => {
            return Err(AppError::message("只能撤销已经完成的操作"));
        }
    }
    if rows
        .iter()
        .any(|row| row.0 >= target_id && row.5 == OperationStatus::UndoFailed.as_str())
    {
        return Err(AppError::message(
            "之后有一条记录上次撤销失败，不能继续回退；请先重试或跳过这条记录",
        ));
    }
    if rows.iter().any(|row| {
        row.0 >= target_id && matches!(row.5.as_str(), "undone" | "redoing") && !row.6.is_empty()
    }) {
        return Err(AppError::message(
            "之后有一条记录上次重做失败，不能继续回退；请先重试重做或清空记录",
        ));
    }

    let selected = rows
        .iter()
        .filter(|row| matches!(row.5.as_str(), "applied" | "undoing"))
        .map(operation_entry)
        .collect::<AppResult<Vec<_>>>()?;
    if selected.is_empty() {
        return Err(AppError::message("没有可撤销的记录"));
    }
    Ok(build_preview(target_id, selected))
}

/// 与 `run_redo` 相同的连续 `undone` 链判定，但不写数据库。
pub fn preview_redo(database: &LibraryDb, target_id: i64) -> AppResult<OperationPreview> {
    let rows = read_operation_rows(
        database,
        "SELECT id, kind, label, created_at, affected, status, error
         FROM operations WHERE id <= ?1 ORDER BY id DESC",
        target_id,
    )?;
    let target = rows
        .first()
        .filter(|row| row.0 == target_id)
        .ok_or_else(|| AppError::message("操作记录不存在"))?;
    match OperationStatus::parse(&target.5)? {
        OperationStatus::Undone | OperationStatus::Redoing => {}
        OperationStatus::Applied => {
            return Err(AppError::message("这条记录还没有撤销，不能重做"));
        }
        OperationStatus::UndoFailed => {
            return Err(AppError::message(
                "这条记录上次撤销失败，需要先处理失败原因",
            ));
        }
        OperationStatus::UndoSkipped => {
            return Err(AppError::message("这条记录已跳过撤销"));
        }
        OperationStatus::Pending | OperationStatus::Undoing => {
            return Err(AppError::message("只能重做已经撤销的操作"));
        }
    }
    let selected = rows
        .iter()
        .take_while(|row| matches!(row.5.as_str(), "undone" | "redoing"))
        .map(operation_entry)
        .collect::<AppResult<Vec<_>>>()?;
    if selected.is_empty() {
        return Err(AppError::message("没有可重做的记录"));
    }
    // 重做按时间从早到晚执行；预览也按这个顺序给出。
    let mut selected = selected;
    selected.reverse();
    Ok(build_preview(target_id, selected))
}

fn build_preview(target_id: i64, entries: Vec<HistoryEntry>) -> OperationPreview {
    let blocked_reason = entries
        .iter()
        .any(|entry| entry.kind == HistoryKind::Optimize)
        .then(|| "压缩操作不可撤销，原图已删除".to_owned());
    OperationPreview {
        target_id,
        steps: entries.len() as u64,
        affected: entries.iter().map(|entry| entry.affected).sum(),
        entries,
        blocked_reason,
    }
}

pub fn pending_summary(database: &LibraryDb) -> AppResult<(u64, u64)> {
    database.with_reader(|connection| {
        let (count, bytes): (i64, i64) = connection.query_row(
            "SELECT count(*), coalesce(sum(file_size), 0) FROM images
             WHERE purge_operation_id IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((count.max(0) as u64, bytes.max(0) as u64))
    })
}

/// 是否有上次异常中断、还没收尾的撤销/重做：`undoing` / `redoing` 由 `recover_on_open` 处理。
pub fn has_interrupted_run(database: &LibraryDb) -> AppResult<bool> {
    database.with_reader(|connection| {
        Ok(connection.query_row(
            "SELECT count(*) FROM operations WHERE status IN ('undoing', 'redoing')",
            [],
            |row| row.get::<_, i64>(0),
        )? > 0)
    })
}

/// 操作记录与撤销暂存的占用。逆数据只读 SQLite 的 BLOB 头长度，不读内容。
pub fn storage_usage(database: &LibraryDb, root: &Path) -> AppResult<HistoryStorage> {
    let (record_count, chunk_count, chunk_bytes) = database.with_reader(|connection| {
        let record_count: i64 =
            connection.query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?;
        let (chunk_count, chunk_bytes): (i64, i64) = connection.query_row(
            "SELECT count(*), coalesce(sum(length(data)), 0) FROM operation_chunks",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((
            record_count.max(0) as u64,
            chunk_count.max(0) as u64,
            chunk_bytes.max(0) as u64,
        ))
    })?;
    let (undo_files, undo_bytes) = undo_usage(&LibraryPaths::new(root).undo);
    Ok(HistoryStorage {
        record_count,
        chunk_count,
        chunk_bytes,
        undo_files,
        undo_bytes,
        total_bytes: chunk_bytes.saturating_add(undo_bytes),
    })
}

fn undo_usage(path: &Path) -> (u64, u64) {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("读取撤销暂存目录失败 {}：{error}", path.display());
            }
            return (0, 0);
        }
    };
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let (child_files, child_bytes) = undo_usage(&entry.path());
            files = files.saturating_add(child_files);
            bytes = bytes.saturating_add(child_bytes);
        } else if file_type.is_file()
            && let Ok(metadata) = entry.metadata()
        {
            files = files.saturating_add(1);
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    (files, bytes)
}

/// 只裁剪 `applied` / `undoFailed` / `undoSkipped`；`pending` / `undoing` 交给启动恢复。
pub fn trim(database: &LibraryDb, max_steps: u32) -> AppResult<Vec<EvictedOperation>> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let evicted = {
            let mut statement = transaction.prepare(
                "SELECT id, kind FROM operations
                 WHERE status IN ('applied', 'undoFailed', 'undoSkipped')
                 ORDER BY id DESC
                 LIMIT -1 OFFSET ?1",
            )?;
            let rows = statement
                .query_map([max_steps as i64], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .map(|(id, kind)| {
                    Ok(EvictedOperation {
                        id,
                        kind: HistoryKind::parse(&kind)?,
                    })
                })
                .collect::<AppResult<Vec<_>>>()?
        };
        for operation in &evicted {
            transaction.execute("DELETE FROM operations WHERE id = ?1", [operation.id])?;
        }
        // 运行审计只跟随仍存在的记录；被裁掉的目标不再有恢复意义。
        transaction.execute(
            "DELETE FROM undo_runs WHERE target_operation_id NOT IN (SELECT id FROM operations)",
            [],
        )?;
        transaction.commit()?;
        Ok(evicted)
    })
}

/// DB 侧的崩溃恢复：`pending` 转为可撤销或删除，收集 `undoing` 与无主待释放批次。
pub fn recover_on_open(database: &LibraryDb) -> AppResult<RecoveryPlan> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending: Vec<(i64, String, i64, i64)> = {
            let mut statement = transaction.prepare(
                "SELECT operations.id, operations.kind, operations.affected,
                        (SELECT count(*) FROM operation_chunks
                         WHERE operation_id = operations.id)
                 FROM operations
                 WHERE operations.status = 'pending'",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, kind, affected, chunks) in pending {
            // 导入的 pending 记录按 origin 标记判断是否有已提交内容；仅合并的批次没有新行，但有快照 chunk。
            let has_content = if kind == HistoryKind::Import.as_str() {
                chunks > 0
                    || transaction.query_row(
                        "SELECT count(*) FROM images WHERE origin_operation_id = ?1 LIMIT 1",
                        [id],
                        |row| row.get::<_, i64>(0),
                    )? > 0
            } else {
                affected > 0 || chunks > 0
            };
            if has_content {
                transaction.execute(
                    "UPDATE operations SET status = 'applied' WHERE id = ?1",
                    [id],
                )?;
            } else {
                transaction.execute("DELETE FROM operations WHERE id = ?1", [id])?;
            }
        }

        let resume_undo: Option<i64> = transaction
            .query_row(
                "SELECT min(id) FROM operations WHERE status = 'undoing'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        // 重做会把整批选中记录先标为 redoing；max(id) 就是这次用户选择的目标，
        // 续做只处理这些记录，不会顺带重做目标之上、用户没选的记录。
        let resume_redo: Option<i64> = transaction
            .query_row(
                "SELECT max(id) FROM operations WHERE status = 'redoing'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        // 上一次撤销没有跑完：进程已经退出，把遗留的 running 行标记失败；
        // 续做会新建一条 run。
        transaction.execute(
            "UPDATE undo_runs SET state = 'failed', finished_at = ?1 WHERE state = 'running'",
            [now_ms()],
        )?;

        let orphan: i64 = transaction.query_row(
            "SELECT
                 (SELECT count(*) FROM images
                  WHERE purge_operation_id IS NOT NULL
                    AND purge_operation_id NOT IN (SELECT id FROM operations))
               + (SELECT count(*) FROM folders
                  WHERE purge_operation_id IS NOT NULL
                    AND purge_operation_id NOT IN (SELECT id FROM operations))",
            [],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        Ok(RecoveryPlan {
            resume_undo,
            resume_redo,
            release_needed: orphan > 0,
        })
    })
}

/// 删除没有对应记录引用的 `.artbank/undo/<id>` 目录。
///
/// 必须在写连接上执行：编辑保存会在自己的写事务里新建暂存目录、提交后 `operations`
/// 行才可见；用读连接枚举会把这种“在建”目录当成无主目录删掉，编辑就永远不可撤销。
pub fn cleanup_undo_dirs(database: &LibraryDb, root: &Path) -> AppResult<()> {
    let undo_dir = LibraryPaths::new(root).undo;
    if !undo_dir.is_dir() {
        return Ok(());
    }
    database.with_writer(|connection| {
        let mut statement = connection.prepare("SELECT id FROM operations")?;
        let known: HashSet<i64> = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        drop(statement);
        for entry in std::fs::read_dir(&undo_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|value| value.parse::<i64>().ok()) else {
                continue;
            };
            if known.contains(&id) {
                continue;
            }
            let path = entry.path();
            if let Err(error) = std::fs::remove_dir_all(&path) {
                log::warn!("清理无主撤销暂存目录失败 {}：{error}", path.display());
            }
        }
        Ok(())
    })
}

pub(super) fn clear_redo_chunks(transaction: &Transaction<'_>, operation_id: i64) -> AppResult<()> {
    transaction.execute(
        "DELETE FROM operation_chunks WHERE operation_id = ?1 AND format = ?2",
        params![operation_id, ChunkFormat::Redo.as_str()],
    )?;
    Ok(())
}

pub(super) fn append_redo(
    transaction: &Transaction<'_>,
    operation_id: i64,
    record: &RedoRecord,
) -> AppResult<()> {
    append_live_in_tx(
        transaction,
        operation_id,
        ChunkFormat::Redo,
        &serde_json::to_vec(record)?,
    )
}

pub(super) fn load_redo_records(
    connection: &Connection,
    operation_id: i64,
) -> AppResult<Vec<RedoRecord>> {
    let mut records = Vec::new();
    for chunk in load_chunks(connection, operation_id)? {
        if chunk.format == ChunkFormat::Redo {
            records.push(serde_json::from_slice(&chunk.data)?);
        }
    }
    Ok(records)
}

/// 清空全部操作记录、运行审计与撤销暂存目录；不删除任何原图，待删文件由单独的释放动作处理。
pub fn clear(database: &LibraryDb, root: &Path) -> AppResult<u64> {
    let deleted = database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM operations", [])?;
        let deleted = transaction.changes();
        transaction.execute("DELETE FROM undo_runs", [])?;
        transaction.commit()?;
        Ok(deleted)
    })?;
    let undo_dir = LibraryPaths::new(root).undo;
    if undo_dir.is_dir() {
        if let Err(error) = std::fs::remove_dir_all(&undo_dir) {
            log::warn!("清理撤销暂存目录失败：{error}");
        } else if let Err(error) = std::fs::create_dir_all(&undo_dir) {
            log::warn!("重建撤销暂存目录失败：{error}");
        }
    }
    Ok(deleted)
}

/// 还有没有可以真正释放的待删除批次。
///
/// 图片行只要还有无主标记（对应记录已不存在）就算：行必须删掉。文件夹行还要看磁盘目录：
/// 目录里还有未纳管文件时这一轮删不掉，保留标记等下次清空回收站再试，不能因此让这里
/// 永远为真——否则每次写入后都会白排一次释放任务。
pub fn has_orphan_purge(database: &LibraryDb, root: &Path) -> AppResult<bool> {
    let (images, folders): (i64, Vec<String>) = database.with_reader(|connection| {
        let images: i64 = connection.query_row(
            "SELECT count(*) FROM images
             WHERE purge_operation_id IS NOT NULL
               AND purge_operation_id NOT IN (SELECT id FROM operations)",
            [],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT path FROM folders
             WHERE purge_operation_id IS NOT NULL
               AND purge_operation_id NOT IN (SELECT id FROM operations)",
        )?;
        let folders = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((images, folders))
    })?;
    if images > 0 {
        return Ok(true);
    }
    let paths = LibraryPaths::new(root);
    Ok(folders.iter().any(|path| {
        matches!(
            metadata::folder_dir_state(&paths, path),
            metadata::FolderDirState::Missing | metadata::FolderDirState::Empty
        )
    }))
}

/// 撤销目标及其之后的全部记录；最新的先撤，取消只在记录之间生效。
/// 撤完的记录不再删除，而是标为 `undone` 留在列表里供重做。
pub fn run_undo(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    target_id: i64,
    task: Arc<TaskHandle>,
) -> AppResult<()> {
    let run_id = start_run(database, target_id)?;
    let run = HistoryRunGuard::new(database, run_id);

    let operations = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, kind, label FROM operations
             WHERE id >= ?1 AND status IN ('applied', 'undoing')
             ORDER BY id DESC",
        )?;
        let rows = statement
            .query_map([target_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, kind, label)| Ok((id, HistoryKind::parse(&kind)?, label)))
            .collect::<AppResult<Vec<_>>>()
    })?;

    if operations.is_empty() {
        run.finish("completed")?;
        return Err(AppError::message("没有可撤销的记录"));
    }

    // 压缩会直接删除原图，无法回退；在标记 undoing 之前拒绝，避免留下假的“撤销失败”。
    if operations
        .iter()
        .any(|(_, kind, _)| *kind == HistoryKind::Optimize)
    {
        run.finish("failed")?;
        return Err(AppError::message("压缩操作不可撤销，原图已删除"));
    }

    task.set_total(operations.len());
    let mut cancelled = false;
    for (id, kind, label) in operations {
        if task.should_cancel() {
            cancelled = true;
            break;
        }
        if let Err(error) = undo_record(database, root, thumbs, &task, id, kind, &label) {
            run.finish("failed")?;
            return Err(error);
        }
    }
    run.finish(if cancelled { "cancelled" } else { "completed" })?;
    task.set_message("");
    Ok(())
}

/// 重试单条撤销失败的记录：只撤这一条，不碰时间线上其他记录。
pub fn retry_undo(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    operation_id: i64,
    task: Arc<TaskHandle>,
) -> AppResult<()> {
    let run_id = start_run(database, operation_id)?;
    let run = HistoryRunGuard::new(database, run_id);
    let (kind, label, status): (String, String, String) = database.with_reader(|connection| {
        Ok(connection.query_row(
            "SELECT kind, label, status FROM operations WHERE id = ?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    })?;
    if status != OperationStatus::UndoFailed.as_str() {
        run.finish("failed")?;
        return Err(AppError::message("只有撤销失败的记录可以重试"));
    }
    task.set_total(1);
    let kind = HistoryKind::parse(&kind)?;
    match undo_record(database, root, thumbs, &task, operation_id, kind, &label) {
        Ok(()) => {
            run.finish("completed")?;
            task.set_message("");
            Ok(())
        }
        Err(error) => {
            run.finish("failed")?;
            Err(error)
        }
    }
}

/// 放弃撤销失败的记录：保留在时间线里，之后不再参与撤销与重做。
pub fn skip_undo(database: &LibraryDb, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM operations WHERE id = ?1",
                [operation_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Err(AppError::message("操作记录不存在"));
        };
        if status != OperationStatus::UndoFailed.as_str() {
            return Err(AppError::message("只有撤销失败的记录可以跳过"));
        }
        transaction.execute(
            "UPDATE operations SET status = 'undoSkipped' WHERE id = ?1",
            [operation_id],
        )?;
        transaction.commit()?;
        Ok(())
    })
}

fn undo_record(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    task: &TaskHandle,
    id: i64,
    kind: HistoryKind,
    label: &str,
) -> AppResult<()> {
    task.set_message(format!("撤销：{label}"));
    // 先记 undoing 再撤：崩溃后按最小的 undoing id 续做，撤销失败后的重试也从这里继续。
    // 只有第一次从 applied 进入时才丢弃旧的重做数据；续做与重试都必须保留上次已写入的快照
    // （导入撤销按批提交，重做要靠这些已提交的快照才知道哪些行已经撤过）。
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status: String =
            transaction.query_row("SELECT status FROM operations WHERE id = ?1", [id], |row| {
                row.get(0)
            })?;
        if status == OperationStatus::Applied.as_str() {
            clear_redo_chunks(&transaction, id)?;
        }
        transaction.execute(
            "UPDATE operations SET status = 'undoing', error = '' WHERE id = ?1",
            [id],
        )?;
        transaction.commit()?;
        Ok(())
    })?;
    match undo_operation(database, root, thumbs, task, id, kind) {
        Ok(()) => {
            database.with_writer(|connection| {
                connection.execute(
                    "UPDATE operations
                     SET status = 'undone', undone_at = ?1, error = ''
                     WHERE id = ?2",
                    params![now_ms(), id],
                )?;
                Ok(())
            })?;
            task.inc_completed();
            Ok(())
        }
        Err(error) => {
            database.with_writer(|connection| {
                connection.execute(
                    "UPDATE operations SET status = 'undoFailed', error = ?1 WHERE id = ?2",
                    params![error.to_string(), id],
                )?;
                Ok(())
            })?;
            Err(AppError::message(format!("撤销「{label}」失败：{error}")))
        }
    }
}

/// 重做当前状态到目标记录之间的全部 `undone` 记录；越早的记录越先做。
/// 选中的记录先整体标为 `redoing`：崩溃后按剩下的记录续做，不会越过用户选的目标。
pub fn run_redo(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    target_id: i64,
    task: Arc<TaskHandle>,
) -> AppResult<()> {
    let run_id = start_run(database, target_id)?;
    let run = HistoryRunGuard::new(database, run_id);

    let operations = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, kind, label, status FROM operations
             WHERE id <= ?1
             ORDER BY id DESC",
        )?;
        let rows = statement
            .query_map([target_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut selected = Vec::new();
        for (id, kind, label, status) in rows {
            if status == OperationStatus::Undone.as_str()
                || status == OperationStatus::Redoing.as_str()
            {
                selected.push((id, HistoryKind::parse(&kind)?, label));
            } else {
                // 撤销块是连续的；遇到已应用或不支持的记录就停在当前状态。
                break;
            }
        }
        selected.reverse();
        Ok(selected)
    })?;

    if operations.is_empty() {
        run.finish("completed")?;
        return Err(AppError::message("没有可重做的记录"));
    }

    let ids: Vec<i64> = operations.iter().map(|(id, _, _)| *id).collect();
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in &ids {
            transaction.execute(
                "UPDATE operations SET status = 'redoing', error = '' WHERE id = ?1",
                [id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    })?;

    task.set_total(operations.len());
    let mut cancelled = false;
    for (index, (id, kind, label)) in operations.iter().enumerate() {
        if task.should_cancel() {
            cancelled = true;
            reset_redoing(database, &ids[index..])?;
            break;
        }
        task.set_message(format!("重做：{label}"));
        match redo_operation(database, root, thumbs, &task, *id, *kind) {
            Ok(()) => {
                database.with_writer(|connection| {
                    connection.execute(
                        "UPDATE operations
                         SET status = 'applied', undone_at = NULL, error = ''
                         WHERE id = ?1",
                        [id],
                    )?;
                    Ok(())
                })?;
                task.inc_completed();
            }
            Err(error) => {
                // 重做失败不会丢掉撤销状态；修正原因后可以再次重做。
                database.with_writer(|connection| {
                    connection.execute(
                        "UPDATE operations SET status = 'undone', error = ?1 WHERE id = ?2",
                        params![error.to_string(), id],
                    )?;
                    Ok(())
                })?;
                reset_redoing(database, &ids[index + 1..])?;
                run.finish("failed")?;
                return Err(AppError::message(format!("重做「{label}」失败：{error}")));
            }
        }
    }
    run.finish(if cancelled { "cancelled" } else { "completed" })?;
    task.set_message("");
    Ok(())
}

fn reset_redoing(database: &LibraryDb, ids: &[i64]) -> AppResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in ids {
            transaction.execute(
                "UPDATE operations SET status = 'undone' WHERE id = ?1 AND status = 'redoing'",
                [id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    })
}

pub(super) fn start_run(database: &LibraryDb, target_id: i64) -> AppResult<i64> {
    database.with_writer(|connection| {
        connection.execute(
            "INSERT INTO undo_runs(target_operation_id, state, started_at)
             VALUES (?1, 'running', ?2)",
            params![target_id, now_ms()],
        )?;
        Ok(connection.last_insert_rowid())
    })
}

fn finish_run(database: &LibraryDb, run_id: i64, state: &str) -> AppResult<()> {
    database.with_writer(|connection| {
        connection.execute(
            "UPDATE undo_runs SET state = ?1, finished_at = ?2 WHERE id = ?3",
            params![state, now_ms(), run_id],
        )?;
        Ok(())
    })
}

/// 撤销/重做运行的兜底：panic 时不能让 `undo_runs` 永远停在 running。
pub(super) struct HistoryRunGuard<'a> {
    database: &'a LibraryDb,
    run_id: i64,
    finished: bool,
}

impl<'a> HistoryRunGuard<'a> {
    pub(super) fn new(database: &'a LibraryDb, run_id: i64) -> Self {
        Self {
            database,
            run_id,
            finished: false,
        }
    }

    fn finish(mut self, state: &str) -> AppResult<()> {
        self.finished = true;
        finish_run(self.database, self.run_id, state)
    }
}

impl Drop for HistoryRunGuard<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Err(error) = finish_run(self.database, self.run_id, "failed") {
            log::warn!("收尾撤销运行 {} 失败：{error}", self.run_id);
        }
    }
}

fn undo_operation(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    task: &TaskHandle,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    match kind {
        HistoryKind::Edit => undo_edit(database, root, thumbs, operation_id)?,
        HistoryKind::Import => undo_import(database, root, task, operation_id)?,
        HistoryKind::Move => undo_move(database, root, operation_id)?,
        HistoryKind::FolderCreate
        | HistoryKind::FolderRename
        | HistoryKind::FolderMove
        | HistoryKind::FolderDelete
        | HistoryKind::FolderReorder
        | HistoryKind::FolderTrash
        | HistoryKind::FolderRestore => undo_folder(database, root, operation_id, kind)?,
        HistoryKind::Trash
        | HistoryKind::Restore
        | HistoryKind::PermanentDelete
        | HistoryKind::EmptyTrash
        | HistoryKind::FolderPurge => undo_visibility(database, operation_id, kind)?,
        HistoryKind::Score => undo_score(database, operation_id)?,
        HistoryKind::Description | HistoryKind::AiDescription => {
            undo_description(database, operation_id)?
        }
        HistoryKind::TagSet
        | HistoryKind::TagAdd
        | HistoryKind::TagRemove
        | HistoryKind::TagRename
        | HistoryKind::TagDelete
        | HistoryKind::TagMerge => undo_tags(database, operation_id, kind)?,
        HistoryKind::CollectionCreate
        | HistoryKind::CollectionRename
        | HistoryKind::CollectionDelete
        | HistoryKind::CollectionReorder
        | HistoryKind::CollectionAdd
        | HistoryKind::CollectionRemove
        | HistoryKind::CollectionCombine
        | HistoryKind::SimilarityGroups => undo_collection(database, operation_id, kind)?,
        HistoryKind::OrphanMove => undo_orphan_move(database, operation_id)?,
        HistoryKind::Optimize => {
            return Err(AppError::message("压缩操作不可撤销，原图已删除"));
        }
    }
    Ok(())
}

fn redo_operation(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    task: &TaskHandle,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    match kind {
        HistoryKind::Edit => redo_edit(database, root, thumbs, operation_id)?,
        HistoryKind::Import => redo_import(database, root, task, operation_id)?,
        HistoryKind::Move => redo_move(database, root, operation_id)?,
        HistoryKind::FolderCreate
        | HistoryKind::FolderRename
        | HistoryKind::FolderMove
        | HistoryKind::FolderDelete
        | HistoryKind::FolderReorder
        | HistoryKind::FolderTrash
        | HistoryKind::FolderRestore => redo_folder(database, root, operation_id, kind)?,
        HistoryKind::Trash
        | HistoryKind::Restore
        | HistoryKind::PermanentDelete
        | HistoryKind::EmptyTrash
        | HistoryKind::FolderPurge => redo_visibility(database, operation_id, kind)?,
        HistoryKind::Score => redo_score(database, operation_id)?,
        HistoryKind::Description | HistoryKind::AiDescription => {
            redo_description(database, operation_id)?
        }
        HistoryKind::TagSet
        | HistoryKind::TagAdd
        | HistoryKind::TagRemove
        | HistoryKind::TagRename
        | HistoryKind::TagDelete
        | HistoryKind::TagMerge => redo_tags(database, operation_id)?,
        HistoryKind::CollectionCreate
        | HistoryKind::CollectionRename
        | HistoryKind::CollectionDelete
        | HistoryKind::CollectionReorder
        | HistoryKind::CollectionAdd
        | HistoryKind::CollectionRemove
        | HistoryKind::CollectionCombine
        | HistoryKind::SimilarityGroups => redo_collection(database, operation_id)?,
        HistoryKind::OrphanMove => redo_orphan_move(database, operation_id)?,
        HistoryKind::Optimize => {
            return Err(AppError::message("压缩操作不可撤销，原图已删除"));
        }
    }
    Ok(())
}

struct OffsetReporter<'a> {
    task: &'a TaskHandle,
    offset: usize,
    total: usize,
}

impl BatchReporter for OffsetReporter<'_> {
    fn report(&self, completed: usize, _total: usize) {
        self.task.report(self.offset + completed, self.total);
    }

    fn cancelled(&self) -> bool {
        self.task.should_cancel()
    }
}

/// 释放没有记录引用的待删除批次；可重复执行。
pub fn run_release(database: &LibraryDb, root: &Path, task: Arc<TaskHandle>) -> AppResult<()> {
    let batches: Vec<(i64, u64)> = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT operation_id, sum(rows) FROM (
                 SELECT purge_operation_id AS operation_id, count(*) AS rows FROM images
                 WHERE purge_operation_id IS NOT NULL
                   AND purge_operation_id NOT IN (SELECT id FROM operations)
                 GROUP BY purge_operation_id
                 UNION ALL
                 SELECT purge_operation_id, count(*) FROM folders
                 WHERE purge_operation_id IS NOT NULL
                   AND purge_operation_id NOT IN (SELECT id FROM operations)
                 GROUP BY purge_operation_id
             )
             GROUP BY operation_id
             ORDER BY operation_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;

    if batches.is_empty() {
        task.set_message("没有待释放的文件");
        return Ok(());
    }

    let total: u64 = batches.iter().map(|(_, count)| *count).sum();
    task.set_total(total as usize);
    let mut offset = 0_usize;
    let mut failures = 0_usize;
    let mut stopped = false;
    for (operation_id, count) in batches {
        if task.should_cancel() {
            // 停在块边界：没处理的行还带着待释放标记，下次释放接着删。
            stopped = true;
            break;
        }
        let reporter = OffsetReporter {
            task: &task,
            offset,
            total: total as usize,
        };
        let report = metadata::purge_by_operation(database, root, &[operation_id], &reporter)?;
        // 释放失败不能只留在报告里：行已经删了，用户只能靠这条失败清单找回文件。
        for failure in &report.failures {
            log::warn!(
                "释放失败 path={} image={:?}：{}",
                failure.path,
                failure.image_id,
                failure.reason
            );
            task.add_failure(
                failure.image_id,
                failure.path.clone(),
                failure.reason.clone(),
            );
            task.inc_failed();
            failures += 1;
        }
        offset += count as usize;
        task.report(offset, total as usize);
    }
    let mut message = String::new();
    if failures > 0 {
        message.push_str(&format!("{failures} 项未能释放，详见失败清单与日志"));
    }
    if stopped {
        if !message.is_empty() {
            message.push_str(" · ");
        }
        message.push_str("已停止，剩余待释放保留到下次");
    }
    task.set_message(message);
    Ok(())
}

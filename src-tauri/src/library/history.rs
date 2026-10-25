//! 图库操作记录与撤销。
//!
//! 每条操作记录与产生它的写操作处在同一个 SQLite 事务里；批量操作只记一条。
//! 撤销按记录 id 线性回退：撤销目标及其之后的所有记录，最新的先撤。
//! 永久删除与清空回收站只写 `purge_operation_id` 标记，记录过期或清空记录后才真正删行删文件。

// 批次 1 只接入回收站与删除类操作；批次 2/3 要用的 live operation、其余记录类型与解码
// 辅助先按计划保留，因此整个模块允许未使用项告警。
#![allow(dead_code)]

use std::{
    borrow::Cow,
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::TaskHandle,
    },
    library::{BatchReporter, db::LibraryDb, metadata, paths::LibraryPaths, service::now_ms},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryKind {
    Import,
    Edit,
    Move,
    Trash,
    Restore,
    PermanentDelete,
    EmptyTrash,
    FolderCreate,
    FolderRename,
    FolderMove,
    FolderDelete,
    FolderReorder,
    Score,
    Description,
    TagSet,
    TagAdd,
    TagRemove,
    TagRename,
    TagDelete,
    CollectionCreate,
    CollectionRename,
    CollectionDelete,
    CollectionReorder,
    CollectionAdd,
    CollectionRemove,
    CollectionCombine,
    SimilarityGroups,
    AiDescription,
}

impl HistoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Edit => "edit",
            Self::Move => "move",
            Self::Trash => "trash",
            Self::Restore => "restore",
            Self::PermanentDelete => "permanentDelete",
            Self::EmptyTrash => "emptyTrash",
            Self::FolderCreate => "folderCreate",
            Self::FolderRename => "folderRename",
            Self::FolderMove => "folderMove",
            Self::FolderDelete => "folderDelete",
            Self::FolderReorder => "folderReorder",
            Self::Score => "score",
            Self::Description => "description",
            Self::TagSet => "tagSet",
            Self::TagAdd => "tagAdd",
            Self::TagRemove => "tagRemove",
            Self::TagRename => "tagRename",
            Self::TagDelete => "tagDelete",
            Self::CollectionCreate => "collectionCreate",
            Self::CollectionRename => "collectionRename",
            Self::CollectionDelete => "collectionDelete",
            Self::CollectionReorder => "collectionReorder",
            Self::CollectionAdd => "collectionAdd",
            Self::CollectionRemove => "collectionRemove",
            Self::CollectionCombine => "collectionCombine",
            Self::SimilarityGroups => "similarityGroups",
            Self::AiDescription => "aiDescription",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        let kind = match value {
            "import" => Self::Import,
            "edit" => Self::Edit,
            "move" => Self::Move,
            "trash" => Self::Trash,
            "restore" => Self::Restore,
            "permanentDelete" => Self::PermanentDelete,
            "emptyTrash" => Self::EmptyTrash,
            "folderCreate" => Self::FolderCreate,
            "folderRename" => Self::FolderRename,
            "folderMove" => Self::FolderMove,
            "folderDelete" => Self::FolderDelete,
            "folderReorder" => Self::FolderReorder,
            "score" => Self::Score,
            "description" => Self::Description,
            "tagSet" => Self::TagSet,
            "tagAdd" => Self::TagAdd,
            "tagRemove" => Self::TagRemove,
            "tagRename" => Self::TagRename,
            "tagDelete" => Self::TagDelete,
            "collectionCreate" => Self::CollectionCreate,
            "collectionRename" => Self::CollectionRename,
            "collectionDelete" => Self::CollectionDelete,
            "collectionReorder" => Self::CollectionReorder,
            "collectionAdd" => Self::CollectionAdd,
            "collectionRemove" => Self::CollectionRemove,
            "collectionCombine" => Self::CollectionCombine,
            "similarityGroups" => Self::SimilarityGroups,
            "aiDescription" => Self::AiDescription,
            other => {
                return Err(AppError::message(format!("未知的操作类型：{other}")));
            }
        };
        Ok(kind)
    }

    /// 含临时文件（如编辑暂存）的操作在记录被裁掉时需要清理 `.artbank/undo/<id>`。
    pub fn keeps_staged_files(self) -> bool {
        self == Self::Edit
    }

    /// 记录被裁掉时，这次操作标记的待删除图片失去撤销可能，要安排释放文件。
    pub fn releases_files(self) -> bool {
        matches!(self, Self::PermanentDelete | Self::EmptyTrash)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationStatus {
    Pending,
    Applied,
    Undoing,
    UndoFailed,
}

impl OperationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Undoing => "undoing",
            Self::UndoFailed => "undoFailed",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "applied" => Ok(Self::Applied),
            "undoing" => Ok(Self::Undoing),
            "undoFailed" => Ok(Self::UndoFailed),
            other => Err(AppError::message(format!("未知的操作状态：{other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkFormat {
    Ids,
    Triples,
    Moves,
    Json,
}

impl ChunkFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ids => "ids",
            Self::Triples => "triples",
            Self::Moves => "moves",
            Self::Json => "json",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "ids" => Ok(Self::Ids),
            "triples" => Ok(Self::Triples),
            "moves" => Ok(Self::Moves),
            "json" => Ok(Self::Json),
            other => Err(AppError::message(format!("未知的记录数据格式：{other}"))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewOperation<'a> {
    pub kind: HistoryKind,
    pub label: Cow<'a, str>,
    pub task_id: Option<&'a str>,
}

impl<'a> NewOperation<'a> {
    pub fn new(kind: HistoryKind, label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            kind,
            label: label.into(),
            task_id: None,
        }
    }
}

struct Chunk {
    format: ChunkFormat,
    data: Vec<u8>,
}

pub struct OperationDraft {
    id: i64,
    affected: u64,
    chunks: Vec<Chunk>,
}

impl OperationDraft {
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn affected(&mut self, n: u64) {
        self.affected = n;
    }

    pub fn add_affected(&mut self, n: u64) {
        self.affected = self.affected.saturating_add(n);
    }

    pub fn ids(&mut self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        let mut data = Vec::with_capacity(ids.len() * 8);
        for id in ids {
            data.extend_from_slice(&id.to_le_bytes());
        }
        self.chunks.push(Chunk {
            format: ChunkFormat::Ids,
            data,
        });
    }

    pub fn triples(&mut self, rows: &[(i64, i64, i64)]) {
        if rows.is_empty() {
            return;
        }
        let mut data = Vec::with_capacity(rows.len() * 24);
        for (first, second, third) in rows {
            data.extend_from_slice(&first.to_le_bytes());
            data.extend_from_slice(&second.to_le_bytes());
            data.extend_from_slice(&third.to_le_bytes());
        }
        self.chunks.push(Chunk {
            format: ChunkFormat::Triples,
            data,
        });
    }

    pub fn moves(&mut self, rows: &[(i64, i64, i64, i64)]) {
        if rows.is_empty() {
            return;
        }
        let mut data = Vec::with_capacity(rows.len() * 32);
        for (id, from, to, modified_at) in rows {
            data.extend_from_slice(&id.to_le_bytes());
            data.extend_from_slice(&from.to_le_bytes());
            data.extend_from_slice(&to.to_le_bytes());
            data.extend_from_slice(&modified_at.to_le_bytes());
        }
        self.chunks.push(Chunk {
            format: ChunkFormat::Moves,
            data,
        });
    }

    pub fn json<T: Serialize>(&mut self, value: &T) -> AppResult<()> {
        self.chunks.push(Chunk {
            format: ChunkFormat::Json,
            data: serde_json::to_vec(value)?,
        });
        Ok(())
    }
}

/// 同步写操作：`work` 与记录写入处在同一个事务里。
/// `affected == 0` 且没有逆数据时整条回滚，不留下空记录。
pub fn write<T>(
    database: &LibraryDb,
    op: NewOperation<'_>,
    work: impl FnOnce(&Transaction<'_>, &mut OperationDraft) -> AppResult<T>,
) -> AppResult<T> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO operations(kind, label, created_at, status, task_id, affected, error)
             VALUES (?1, ?2, ?3, 'pending', ?4, 0, '')",
            params![op.kind.as_str(), op.label.as_ref(), now_ms(), op.task_id],
        )?;
        let mut draft = OperationDraft {
            id: transaction.last_insert_rowid(),
            affected: 0,
            chunks: Vec::new(),
        };
        let value = work(&transaction, &mut draft)?;
        if draft.affected == 0 && draft.chunks.is_empty() {
            // 事务随 drop 回滚。
            return Ok(value);
        }
        transaction.execute(
            "UPDATE operations SET status = 'applied', affected = ?1 WHERE id = ?2",
            params![draft.affected as i64, draft.id],
        )?;
        insert_chunks(&transaction, draft.id, &draft.chunks)?;
        transaction.commit()?;
        Ok(value)
    })
}

/// 长任务先建 `pending` 记录，再在每批写入时追加逆数据；任务结束后 `finish_live`。
pub fn begin_live(database: &LibraryDb, op: NewOperation<'_>) -> AppResult<i64> {
    database.with_writer(|connection| {
        connection.execute(
            "INSERT INTO operations(kind, label, created_at, status, task_id, affected, error)
             VALUES (?1, ?2, ?3, 'pending', ?4, 0, '')",
            params![op.kind.as_str(), op.label.as_ref(), now_ms(), op.task_id],
        )?;
        Ok(connection.last_insert_rowid())
    })
}

pub fn append_live_ids(database: &LibraryDb, op_id: i64, ids: &[i64]) -> AppResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut data = Vec::with_capacity(ids.len() * 8);
    for id in ids {
        data.extend_from_slice(&id.to_le_bytes());
    }
    append_live_chunk(database, op_id, ChunkFormat::Ids, &data)
}

pub fn append_live_json<T: Serialize>(
    database: &LibraryDb,
    op_id: i64,
    value: &T,
) -> AppResult<()> {
    append_live_chunk(
        database,
        op_id,
        ChunkFormat::Json,
        &serde_json::to_vec(value)?,
    )
}

fn append_live_chunk(
    database: &LibraryDb,
    op_id: i64,
    format: ChunkFormat,
    data: &[u8],
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        append_live_in_tx(&transaction, op_id, format, data)?;
        transaction.commit()?;
        Ok(())
    })
}

pub fn add_live_affected(database: &LibraryDb, op_id: i64, n: u64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        add_live_affected_in_tx(&transaction, op_id, n)?;
        transaction.commit()?;
        Ok(())
    })
}

/// 已持有写事务（导入/描述批次内部）时使用，避免再次加写锁造成死锁。
pub fn append_live_in_tx(
    tx: &Transaction<'_>,
    op_id: i64,
    format: ChunkFormat,
    data: &[u8],
) -> AppResult<()> {
    let seq: i64 = tx.query_row(
        "SELECT coalesce(max(seq), -1) + 1 FROM operation_chunks WHERE operation_id = ?1",
        [op_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO operation_chunks(operation_id, seq, format, data) VALUES (?1, ?2, ?3, ?4)",
        params![op_id, seq, format.as_str(), data],
    )?;
    Ok(())
}

pub fn add_live_affected_in_tx(tx: &Transaction<'_>, op_id: i64, n: u64) -> AppResult<()> {
    tx.execute(
        "UPDATE operations SET affected = affected + ?1 WHERE id = ?2",
        params![n as i64, op_id],
    )?;
    Ok(())
}

/// 任务结束（成功、取消或失败）时收尾；没有任何已提交内容时删除空记录。
pub fn finish_live(database: &LibraryDb, op_id: i64, error: &str) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (affected, chunks): (i64, i64) = transaction.query_row(
            "SELECT operations.affected,
                    (SELECT count(*) FROM operation_chunks WHERE operation_id = operations.id)
             FROM operations WHERE id = ?1",
            [op_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if affected == 0 && chunks == 0 {
            transaction.execute("DELETE FROM operations WHERE id = ?1", [op_id])?;
        } else {
            transaction.execute(
                "UPDATE operations SET status = 'applied', error = ?1 WHERE id = ?2",
                params![error, op_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: i64,
    pub kind: HistoryKind,
    pub label: String,
    pub created_at: i64,
    pub affected: u64,
    pub status: OperationStatus,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub max_steps: u32,
    pub pending_purge_count: u64,
    pub pending_purge_bytes: u64,
    pub can_undo: bool,
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EvictedOperation {
    pub id: i64,
    pub kind: HistoryKind,
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryPlan {
    /// 崩溃前 `undoing` 的记录里最小的 id；有值时启动后要续做撤销。
    pub resume_undo: Option<i64>,
    /// 是否存在没有记录引用的待释放批次。
    pub release_needed: bool,
}

impl RecoveryPlan {
    pub fn is_empty(&self) -> bool {
        self.resume_undo.is_none() && !self.release_needed
    }
}

pub fn list(database: &LibraryDb) -> AppResult<Vec<HistoryEntry>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, kind, label, created_at, affected, status, error
             FROM operations
             WHERE status IN ('applied', 'undoFailed')
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

/// 只裁剪 `applied` / `undo_failed`；`pending` / `undoing` 交给启动恢复。
pub fn trim(database: &LibraryDb, max_steps: u32) -> AppResult<Vec<EvictedOperation>> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let evicted = {
            let mut statement = transaction.prepare(
                "SELECT id, kind FROM operations
                 WHERE status IN ('applied', 'undoFailed')
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
        transaction.commit()?;
        Ok(evicted)
    })
}

/// DB 侧的崩溃恢复：`pending` 转为可撤销或删除，收集 `undoing` 与无主待释放批次。
pub fn recover_on_open(database: &LibraryDb) -> AppResult<RecoveryPlan> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending: Vec<(i64, i64, i64)> = {
            let mut statement = transaction.prepare(
                "SELECT operations.id, operations.affected,
                        (SELECT count(*) FROM operation_chunks
                         WHERE operation_id = operations.id)
                 FROM operations
                 WHERE operations.status = 'pending'",
            )?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, affected, chunks) in pending {
            if affected > 0 || chunks > 0 {
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
        // 上一次撤销没有跑完：进程已经退出，把遗留的 running 行标记失败；
        // 续做会新建一条 run。
        transaction.execute(
            "UPDATE undo_runs SET state = 'failed', finished_at = ?1 WHERE state = 'running'",
            [now_ms()],
        )?;

        let orphan: i64 = transaction.query_row(
            "SELECT count(*) FROM images
             WHERE purge_operation_id IS NOT NULL
               AND purge_operation_id NOT IN (SELECT id FROM operations)",
            [],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        Ok(RecoveryPlan {
            resume_undo,
            release_needed: orphan > 0,
        })
    })
}

/// 删除没有对应记录引用的 `.artbank/undo/<id>` 目录。
pub fn cleanup_undo_dirs(database: &LibraryDb, root: &Path) -> AppResult<()> {
    let known: HashSet<i64> = database.with_reader(|connection| {
        let mut statement = connection.prepare("SELECT id FROM operations")?;
        let ids = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(ids)
    })?;
    let undo_dir = LibraryPaths::new(root).undo;
    if !undo_dir.is_dir() {
        return Ok(());
    }
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
}

pub fn undo_dir(root: &Path, operation_id: i64) -> PathBuf {
    LibraryPaths::new(root).undo.join(operation_id.to_string())
}

fn remove_undo_dir(root: &Path, operation_id: i64) {
    let path = undo_dir(root, operation_id);
    if path.exists()
        && let Err(error) = std::fs::remove_dir_all(&path)
    {
        log::warn!("清理撤销暂存目录失败 {}：{error}", path.display());
    }
}

/// 清空全部操作记录，并删除撤销暂存目录；调用方随后安排待释放文件清理。
pub fn clear(database: &LibraryDb, root: &Path) -> AppResult<u64> {
    let deleted = database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM operations", [])?;
        let deleted = transaction.changes();
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

pub fn has_orphan_purge(database: &LibraryDb) -> AppResult<bool> {
    database.with_reader(|connection| {
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM images
             WHERE purge_operation_id IS NOT NULL
               AND purge_operation_id NOT IN (SELECT id FROM operations)
             LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    })
}

/// 撤销目标及其之后的全部记录；最新的先撤，取消只在记录之间生效。
pub fn run_undo(
    database: &LibraryDb,
    root: &Path,
    target_id: i64,
    task: Arc<TaskHandle>,
) -> AppResult<()> {
    let run_id = database.with_writer(|connection| {
        connection.execute(
            "INSERT INTO undo_runs(target_operation_id, state, started_at)
             VALUES (?1, 'running', ?2)",
            params![target_id, now_ms()],
        )?;
        Ok(connection.last_insert_rowid())
    })?;

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
        finish_run(database, run_id, "completed")?;
        return Err(AppError::message("没有可撤销的记录"));
    }

    task.set_total(operations.len());
    let mut cancelled = false;
    for (id, kind, label) in operations {
        if task.should_cancel() {
            cancelled = true;
            break;
        }
        task.set_message(format!("撤销：{label}"));
        // 先记 undoing 再撤：崩溃后按最小的 undoing id 续做；已撤完的记录行已删除，不会被重复处理。
        database.with_writer(|connection| {
            connection.execute(
                "UPDATE operations SET status = 'undoing' WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })?;
        match undo_operation(database, root, id, kind) {
            Ok(()) => task.inc_completed(),
            Err(error) => {
                database.with_writer(|connection| {
                    connection.execute(
                        "UPDATE operations SET status = 'undoFailed', error = ?1 WHERE id = ?2",
                        params![error.to_string(), id],
                    )?;
                    Ok(())
                })?;
                finish_run(database, run_id, "failed")?;
                return Err(AppError::message(format!("撤销「{label}」失败：{error}")));
            }
        }
    }
    finish_run(
        database,
        run_id,
        if cancelled { "cancelled" } else { "completed" },
    )?;
    task.set_message("");
    Ok(())
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

fn undo_operation(
    database: &LibraryDb,
    root: &Path,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let chunks = load_chunks(&transaction, operation_id)?;
        match kind {
            HistoryKind::Trash | HistoryKind::Restore => {
                for chunk in chunks
                    .iter()
                    .filter(|chunk| chunk.format == ChunkFormat::Triples)
                {
                    for (image_id, old_deleted_at, old_modified_at) in decode_triples(&chunk.data)?
                    {
                        let deleted_at = if old_deleted_at == 0 {
                            None
                        } else {
                            Some(old_deleted_at)
                        };
                        // 待释放图片由更晚的记录负责恢复；这里不把它变回正常状态。
                        transaction.execute(
                            "UPDATE images
                             SET deleted_at = ?1, modified_at = ?2
                             WHERE id = ?3 AND purge_operation_id IS NULL",
                            params![deleted_at, old_modified_at, image_id],
                        )?;
                    }
                }
            }
            HistoryKind::PermanentDelete | HistoryKind::EmptyTrash => {
                transaction.execute(
                    "UPDATE images SET purge_operation_id = NULL WHERE purge_operation_id = ?1",
                    [operation_id],
                )?;
            }
            other => {
                log::warn!("记录 {operation_id} 的类型 {} 暂不支持撤销", other.as_str());
                return Err(AppError::message("这类操作暂时不能撤销"));
            }
        }
        transaction.execute("DELETE FROM operations WHERE id = ?1", [operation_id])?;
        transaction.commit()?;
        Ok(())
    })?;
    remove_undo_dir(root, operation_id);
    Ok(())
}

fn load_chunks(transaction: &Transaction<'_>, operation_id: i64) -> AppResult<Vec<Chunk>> {
    let mut statement = transaction.prepare(
        "SELECT format, data FROM operation_chunks WHERE operation_id = ?1 ORDER BY seq",
    )?;
    let rows = statement
        .query_map([operation_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(format, data)| {
            Ok(Chunk {
                format: ChunkFormat::parse(&format)?,
                data,
            })
        })
        .collect()
}

fn decode_ids(bytes: &[u8]) -> AppResult<Vec<i64>> {
    if bytes.len() % 8 != 0 {
        return Err(AppError::message("记录数据长度无效"));
    }
    Ok(bytes
        .chunks_exact(8)
        .map(|slot| i64::from_le_bytes(slot.try_into().expect("8 字节")))
        .collect())
}

fn decode_triples(bytes: &[u8]) -> AppResult<Vec<(i64, i64, i64)>> {
    if bytes.len() % 24 != 0 {
        return Err(AppError::message("记录数据长度无效"));
    }
    Ok(bytes
        .chunks_exact(24)
        .map(|slot| {
            (
                i64::from_le_bytes(slot[0..8].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[8..16].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[16..24].try_into().expect("8 字节")),
            )
        })
        .collect())
}

fn decode_moves(bytes: &[u8]) -> AppResult<Vec<(i64, i64, i64, i64)>> {
    if bytes.len() % 32 != 0 {
        return Err(AppError::message("记录数据长度无效"));
    }
    Ok(bytes
        .chunks_exact(32)
        .map(|slot| {
            (
                i64::from_le_bytes(slot[0..8].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[8..16].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[16..24].try_into().expect("8 字节")),
                i64::from_le_bytes(slot[24..32].try_into().expect("8 字节")),
            )
        })
        .collect())
}

fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> AppResult<T> {
    Ok(serde_json::from_slice(bytes)?)
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
            "SELECT purge_operation_id, count(*) FROM images
             WHERE purge_operation_id IS NOT NULL
               AND purge_operation_id NOT IN (SELECT id FROM operations)
             GROUP BY purge_operation_id
             ORDER BY purge_operation_id",
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
    for (operation_id, _) in batches {
        if task.should_cancel() {
            break;
        }
        let reporter = OffsetReporter {
            task: &task,
            offset,
            total: total as usize,
        };
        let report = metadata::purge_by_operation(database, root, &[operation_id], &reporter)?;
        offset += report.deleted as usize;
        task.report(offset, total as usize);
    }
    task.set_message("");
    Ok(())
}

fn insert_chunks(
    transaction: &Transaction<'_>,
    operation_id: i64,
    chunks: &[Chunk],
) -> AppResult<()> {
    for (seq, chunk) in chunks.iter().enumerate() {
        transaction.execute(
            "INSERT INTO operation_chunks(operation_id, seq, format, data)
             VALUES (?1, ?2, ?3, ?4)",
            params![operation_id, seq as i64, chunk.format.as_str(), chunk.data],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::{
        app::tasks::TaskKind,
        library::{NoopReporter, import::ensure_folder, service},
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        root: PathBuf,
        folder: i64,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempdir().unwrap();
            let root = temp.path().join("library");
            let (session, _) = service::create(&root, "历史测试", 1).unwrap();
            let folder = session
                .db
                .with_writer(|connection| ensure_folder(connection, "人物", 1))
                .unwrap();
            Self {
                _temp: temp,
                session,
                root,
                folder,
            }
        }

        fn db(&self) -> &LibraryDb {
            &self.session.db
        }

        fn insert_image(&self, seed: &str) -> i64 {
            let hash = format!("{seed:0>64}");
            self.session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, 'jpg', ?2, 100, 10, 10, 'jpeg', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        params![hash, self.folder],
                    )?;
                    Ok(connection.last_insert_rowid())
                })
                .unwrap()
        }

        fn write_content(&self, seed: &str) -> PathBuf {
            let hash = format!("{seed:0>64}");
            let path = LibraryPaths::new(&self.root).content_file("人物", &format!("{hash}.jpg"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"content").unwrap();
            path
        }
    }

    fn task() -> Arc<TaskHandle> {
        TaskHandle::new("undo-test", TaskKind::Undo, "测试")
    }

    #[test]
    fn write_rolls_back_empty_operations() {
        let fixture = Fixture::new();
        write(
            fixture.db(),
            NewOperation::new(HistoryKind::Trash, "移入回收站"),
            |_, _| Ok(0_u64),
        )
        .unwrap();
        assert!(list(fixture.db()).unwrap().is_empty());
    }

    #[test]
    fn trims_oldest_records_and_keeps_newest() {
        let fixture = Fixture::new();
        for index in 0..5 {
            write(
                fixture.db(),
                NewOperation::new(HistoryKind::Trash, format!("记录 {index}")),
                |_, draft| {
                    draft.affected(1);
                    draft.ids(&[index + 1]);
                    Ok(())
                },
            )
            .unwrap();
        }
        let evicted = trim(fixture.db(), 2).unwrap();
        assert_eq!(evicted.len(), 3);
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "记录 4");
        assert_eq!(entries[1].label, "记录 3");
    }

    #[test]
    fn undoes_trash_and_restore_in_linear_order() {
        let fixture = Fixture::new();
        let first = fixture.insert_image("a");
        let second = fixture.insert_image("b");

        metadata::set_trashed(fixture.db(), &[first, second], true).unwrap();
        let trashed = list(fixture.db()).unwrap();
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].kind, HistoryKind::Trash);
        assert_eq!(trashed[0].affected, 2);

        metadata::set_trashed(fixture.db(), &[first], false).unwrap();
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, HistoryKind::Restore);

        run_undo(fixture.db(), &fixture.root, entries[1].id, task()).unwrap();
        let rows: Vec<(i64, Option<i64>)> = fixture
            .db()
            .with_reader(|connection| {
                let mut statement =
                    connection.prepare("SELECT id, deleted_at FROM images ORDER BY id")?;
                let rows = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(rows, vec![(first, None), (second, None)]);
        assert!(list(fixture.db()).unwrap().is_empty());
    }

    #[test]
    fn permanent_delete_keeps_rows_until_release_and_undo_clears_marker() {
        let fixture = Fixture::new();
        let image_id = fixture.insert_image("a");
        let path = fixture.write_content("a");
        metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();

        let operation = list(fixture.db()).unwrap();
        let report = metadata::delete_images(fixture.db(), &[image_id], &NoopReporter).unwrap();
        assert!(report.deferred);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.files_removed, 0);
        assert!(path.exists());

        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries[0].kind, HistoryKind::PermanentDelete);

        run_undo(fixture.db(), &fixture.root, entries[0].id, task()).unwrap();
        let deleted_at: Option<i64> = fixture
            .db()
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT deleted_at FROM images WHERE id = ?1",
                    [image_id],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert!(deleted_at.is_some());
        assert!(path.exists());
        assert_eq!(list(fixture.db()).unwrap().len(), 1);
        assert_eq!(list(fixture.db()).unwrap()[0].id, operation[0].id);
    }

    #[test]
    fn release_removes_orphan_purge_batches() {
        let fixture = Fixture::new();
        let image_id = fixture.insert_image("a");
        let path = fixture.write_content("a");
        metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
        metadata::empty_trash(fixture.db(), &NoopReporter).unwrap();
        assert!(path.exists());

        // 记录被裁掉后，待释放批次变成无主。
        trim(fixture.db(), 0).unwrap();
        assert!(has_orphan_purge(fixture.db()).unwrap());

        run_release(fixture.db(), &fixture.root, task()).unwrap();
        assert!(!path.exists());
        let count: i64 = fixture
            .db()
            .with_reader(|connection| {
                Ok(connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(count, 0);
        assert!(!has_orphan_purge(fixture.db()).unwrap());
    }

    #[test]
    fn recovers_pending_and_undoing_records() {
        let fixture = Fixture::new();
        let pending_empty = begin_live(
            fixture.db(),
            NewOperation::new(HistoryKind::Import, "空导入"),
        )
        .unwrap();
        let pending_content = begin_live(
            fixture.db(),
            NewOperation::new(HistoryKind::Import, "有内容导入"),
        )
        .unwrap();
        add_live_affected(fixture.db(), pending_content, 1).unwrap();
        let undoing = begin_live(
            fixture.db(),
            NewOperation::new(HistoryKind::Trash, "中断的撤销"),
        )
        .unwrap();
        fixture
            .db()
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE operations SET status = 'undoing' WHERE id = ?1",
                    [undoing],
                )?;
                Ok(())
            })
            .unwrap();

        let plan = recover_on_open(fixture.db()).unwrap();
        assert_eq!(plan.resume_undo, Some(undoing));
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, pending_content);
        let remaining: i64 =
            fixture
                .db()
                .with_reader(|connection| {
                    Ok(connection
                        .query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?)
                })
                .unwrap();
        assert_eq!(remaining, 2);
        assert!(entries.iter().all(|entry| entry.id != pending_empty));
    }

    #[test]
    fn history_survives_reopen() {
        let fixture = Fixture::new();
        write(
            fixture.db(),
            NewOperation::new(HistoryKind::Trash, "移入回收站"),
            |_, draft| {
                draft.affected(1);
                draft.triples(&[(1, 0, 42)]);
                Ok(())
            },
        )
        .unwrap();
        let Fixture {
            _temp: temp,
            session,
            root,
            folder: _,
        } = fixture;
        drop(session);
        let reopened = service::open(&root, 1).unwrap();
        let entries = list(&reopened.db).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "移入回收站");
        assert_eq!(entries[0].affected, 1);
        drop(temp);
    }

    #[test]
    fn cancellation_stops_between_records() {
        let fixture = Fixture::new();
        let image_id = fixture.insert_image("a");
        metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
        let entries = list(fixture.db()).unwrap();
        let handle = task();
        handle.request_cancel();
        run_undo(fixture.db(), &fixture.root, entries[0].id, handle).unwrap();
        assert_eq!(list(fixture.db()).unwrap().len(), 1);
        assert!(
            metadata::image_details(fixture.db(), &fixture.root, image_id)
                .unwrap()
                .deleted_at
                .is_some()
        );
    }

    #[test]
    fn cleanup_removes_undo_dirs_without_operations() {
        let fixture = Fixture::new();
        let operation = write(
            fixture.db(),
            NewOperation::new(HistoryKind::Edit, "编辑"),
            |_, draft| {
                draft.affected(1);
                Ok(draft.id())
            },
        )
        .unwrap();
        let kept = undo_dir(&fixture.root, operation);
        let orphan = undo_dir(&fixture.root, operation + 998);
        fs::create_dir_all(&kept).unwrap();
        fs::create_dir_all(&orphan).unwrap();
        cleanup_undo_dirs(fixture.db(), &fixture.root).unwrap();
        assert!(kept.exists());
        assert!(!orphan.exists());
    }
}

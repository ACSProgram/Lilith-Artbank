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
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::TaskHandle,
    },
    imageops::thumbnail,
    import::decode,
    library::{
        BatchReporter, content, db::LibraryDb, folders, import as repository, metadata,
        paths::LibraryPaths, service::now_ms, thumbs::ThumbPack, viewer,
    },
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
    /// 已撤销并保留在列表里；可以重做。
    Undone,
    /// 重做进行中；崩溃后从这里续做。
    Redoing,
    UndoFailed,
}

impl OperationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Undoing => "undoing",
            Self::Undone => "undone",
            Self::Redoing => "redoing",
            Self::UndoFailed => "undoFailed",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "applied" => Ok(Self::Applied),
            "undoing" => Ok(Self::Undoing),
            "undone" => Ok(Self::Undone),
            "redoing" => Ok(Self::Redoing),
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
    /// 撤销时捕获的“当前状态”，只用于重做；原 chunks 保持描述撤销方向。
    Redo,
}

impl ChunkFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ids => "ids",
            Self::Triples => "triples",
            Self::Moves => "moves",
            Self::Json => "json",
            Self::Redo => "redo",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "ids" => Ok(Self::Ids),
            "triples" => Ok(Self::Triples),
            "moves" => Ok(Self::Moves),
            "json" => Ok(Self::Json),
            "redo" => Ok(Self::Redo),
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

/// 记录格式：文件夹创建时新建的行；撤销时按深度从深到浅删除。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRef {
    pub id: i64,
    pub path: String,
}

/// 记录格式：删除空文件夹前的内容；撤销时用原 id 重建。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDeleted {
    pub id: i64,
    pub path: String,
    pub created_at: i64,
    pub sort_index: i64,
}

/// 记录格式：文件夹重命名/移动的旧路径与新路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderPathChange {
    pub id: i64,
    pub old_path: String,
    pub new_path: String,
    pub old_sort_index: i64,
    pub new_sort_index: i64,
}

/// 记录格式：同级排序前每个文件夹的 `sort_index`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderReorder {
    pub rows: Vec<(i64, i64)>,
}

/// 记录格式：编辑前的完整内容快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditSnapshot {
    pub image_id: i64,
    pub folder_path: String,
    pub content_hash: String,
    pub file_ext: String,
    pub file_size: i64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub has_alpha: bool,
    pub phash: Vec<u8>,
    pub colors4: Vec<u8>,
    pub colors8: Vec<u8>,
    pub modified_at: i64,
    pub new_hash: String,
    pub new_file_ext: String,
}

/// 导入合并项的快照：撤销时恢复评分、修改时间与标签关系。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportMerged {
    pub image_id: i64,
    pub old_score: i64,
    pub old_modified_at: i64,
    pub old_tag_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTag {
    pub id: i64,
    pub name: String,
}

/// 导入批次在每批事务里追加的逆数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ImportRecord {
    Folders { items: Vec<FolderRef> },
    Tags { items: Vec<ImportTag> },
    Merged { items: Vec<ImportMerged> },
}

/// 撤销时捕获的当前状态；重做只读这些 chunks，不改写原始逆数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RedoRecord {
    ImageStates { items: Vec<ImageState> },
    PurgeIds { ids: Vec<i64> },
    MoveStates { items: Vec<MoveState> },
    FolderRows { items: Vec<FolderRow> },
    FolderSort { rows: Vec<(i64, i64)> },
    EditState { state: EditRedoState },
    ImportImages { items: Vec<ImportRedoImage> },
    ImportTags { items: Vec<ImportRedoTag> },
    ImportMerged { items: Vec<ImportRedoMerged> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageState {
    pub image_id: i64,
    pub deleted_at: Option<i64>,
    pub modified_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveState {
    pub image_id: i64,
    pub from_folder_id: i64,
    pub to_folder_id: i64,
    pub new_modified_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRow {
    pub id: i64,
    pub path: String,
    pub created_at: i64,
    pub sort_index: i64,
}

/// 编辑撤销前的内容；重做时用它恢复行与文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditRedoState {
    pub image_id: i64,
    pub folder_path: String,
    pub content_hash: String,
    pub file_ext: String,
    pub file_size: i64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub has_alpha: bool,
    pub phash: Vec<u8>,
    pub colors4: Vec<u8>,
    pub colors8: Vec<u8>,
    pub modified_at: i64,
}

/// 导入撤销时删除的完整行；重做按原 id 插回。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedRow {
    pub id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub folder_id: i64,
    pub file_size: i64,
    pub width: i64,
    pub height: i64,
    pub format: String,
    pub has_alpha: bool,
    pub phash: Vec<u8>,
    pub colors4: Vec<u8>,
    pub colors8: Vec<u8>,
    pub score: i64,
    pub description: String,
    pub description_source: String,
    pub description_model: String,
    pub imported_at: i64,
    pub modified_at: i64,
    pub deleted_at: Option<i64>,
    pub thumb_segment: i64,
    pub thumb_offset: i64,
    pub thumb_length: i64,
    pub thumb_format: String,
    pub thumb_width: i64,
    pub thumb_height: i64,
    pub thumb_version: i64,
    pub origin_operation_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRedoImage {
    pub row: ImportedRow,
    pub folder_path: String,
    pub tag_ids: Vec<i64>,
    /// `(collectionId, position)`
    pub collection_items: Vec<(i64, i64)>,
    /// 暂存文件名；原图在撤销时已经缺失则为 `None`。
    pub staged_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRedoTag {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRedoMerged {
    pub image_id: i64,
    pub score: i64,
    pub modified_at: i64,
    pub tag_ids: Vec<i64>,
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
        // 新的写入开启新时间线：被撤销的灰记录与它们的暂存文件都不再可重做。
        discard_redo_branch(&transaction)?;
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
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        discard_redo_branch(&transaction)?;
        transaction.execute(
            "INSERT INTO operations(kind, label, created_at, status, task_id, affected, error)
             VALUES (?1, ?2, ?3, 'pending', ?4, 0, '')",
            params![op.kind.as_str(), op.label.as_ref(), now_ms(), op.task_id],
        )?;
        let id = transaction.last_insert_rowid();
        transaction.commit()?;
        Ok(id)
    })
}

fn discard_redo_branch(transaction: &Transaction<'_>) -> AppResult<()> {
    transaction.execute(
        "DELETE FROM operations WHERE status IN ('undone', 'redoing')",
        [],
    )?;
    Ok(())
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
    /// 崩溃前 `redoing` 的记录里最小的 id；有值时启动后要续做重做。
    pub resume_redo: Option<i64>,
    /// 是否存在没有记录引用的待释放批次。
    pub release_needed: bool,
}

impl RecoveryPlan {
    pub fn is_empty(&self) -> bool {
        self.resume_undo.is_none() && self.resume_redo.is_none() && !self.release_needed
    }
}

pub fn list(database: &LibraryDb) -> AppResult<Vec<HistoryEntry>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, kind, label, created_at, affected, status, error
             FROM operations
             WHERE status IN ('applied', 'undoFailed', 'undone', 'redoing')
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
            "SELECT count(*) FROM images
             WHERE purge_operation_id IS NOT NULL
               AND purge_operation_id NOT IN (SELECT id FROM operations)",
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

fn clear_redo_chunks(transaction: &Transaction<'_>, operation_id: i64) -> AppResult<()> {
    transaction.execute(
        "DELETE FROM operation_chunks WHERE operation_id = ?1 AND format = ?2",
        params![operation_id, ChunkFormat::Redo.as_str()],
    )?;
    Ok(())
}

fn append_redo(
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

fn load_redo_records(connection: &Connection, operation_id: i64) -> AppResult<Vec<RedoRecord>> {
    let mut records = Vec::new();
    for chunk in load_chunks(connection, operation_id)? {
        if chunk.format == ChunkFormat::Redo {
            records.push(serde_json::from_slice(&chunk.data)?);
        }
    }
    Ok(records)
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
/// 撤完的记录不再删除，而是标为 `undone` 留在列表里供重做。
pub fn run_undo(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    target_id: i64,
    task: Arc<TaskHandle>,
) -> AppResult<()> {
    let run_id = start_run(database, target_id)?;

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
        // 先记 undoing 再撤：崩溃后按最小的 undoing id 续做。
        // 只有第一次从 applied 进入时才丢弃旧的重做数据；续做必须保留上次已写入的快照。
        database.with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let status: String = transaction.query_row(
                "SELECT status FROM operations WHERE id = ?1",
                [id],
                |row| row.get(0),
            )?;
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
        match undo_operation(database, root, thumbs, &task, id, kind) {
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
            }
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
        finish_run(database, run_id, "completed")?;
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
                finish_run(database, run_id, "failed")?;
                return Err(AppError::message(format!("重做「{label}」失败：{error}")));
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

fn start_run(database: &LibraryDb, target_id: i64) -> AppResult<i64> {
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
        | HistoryKind::FolderReorder => undo_folder(database, root, operation_id, kind)?,
        HistoryKind::Trash
        | HistoryKind::Restore
        | HistoryKind::PermanentDelete
        | HistoryKind::EmptyTrash => undo_visibility(database, operation_id, kind)?,
        other => {
            log::warn!("记录 {operation_id} 的类型 {} 暂不支持撤销", other.as_str());
            return Err(AppError::message("这类操作暂时不能撤销"));
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
        | HistoryKind::FolderReorder => redo_folder(database, root, operation_id, kind)?,
        HistoryKind::Trash
        | HistoryKind::Restore
        | HistoryKind::PermanentDelete
        | HistoryKind::EmptyTrash => redo_visibility(database, operation_id, kind)?,
        other => {
            log::warn!("记录 {operation_id} 的类型 {} 暂不支持重做", other.as_str());
            return Err(AppError::message("这类操作暂时不能重做"));
        }
    }
    Ok(())
}

/// 回收站/删除类：撤销时把当前状态写进 redo chunk，重做时原样写回。
fn undo_visibility(database: &LibraryDb, operation_id: i64, kind: HistoryKind) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let chunks = load_chunks(&transaction, operation_id)?;
        match kind {
            HistoryKind::Trash | HistoryKind::Restore => {
                for chunk in chunks
                    .iter()
                    .filter(|chunk| chunk.format == ChunkFormat::Triples)
                {
                    for batch in decode_triples(&chunk.data)?.chunks(400) {
                        let marks = crate::library::placeholders(batch.len());
                        let mut statement = transaction.prepare(&format!(
                            "SELECT id, deleted_at, modified_at FROM images
                             WHERE id IN ({marks}) AND purge_operation_id IS NULL"
                        ))?;
                        let current = statement
                            .query_map(
                                rusqlite::params_from_iter(batch.iter().map(|(id, _, _)| id)),
                                |row| {
                                    Ok((
                                        row.get::<_, i64>(0)?,
                                        row.get::<_, Option<i64>>(1)?,
                                        row.get::<_, i64>(2)?,
                                    ))
                                },
                            )?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        drop(statement);

                        let mut redo = Vec::new();
                        for (image_id, old_deleted_at, old_modified_at) in batch {
                            let old_deleted_at = if *old_deleted_at == 0 {
                                None
                            } else {
                                Some(*old_deleted_at)
                            };
                            if let Some((_, deleted_at, modified_at)) =
                                current.iter().find(|(id, _, _)| id == image_id)
                                && (*deleted_at != old_deleted_at
                                    || *modified_at != *old_modified_at)
                            {
                                redo.push(ImageState {
                                    image_id: *image_id,
                                    deleted_at: *deleted_at,
                                    modified_at: *modified_at,
                                });
                            }
                            // 待释放图片由更晚的记录负责恢复；这里不把它变回正常状态。
                            transaction.execute(
                                "UPDATE images
                                 SET deleted_at = ?1, modified_at = ?2
                                 WHERE id = ?3 AND purge_operation_id IS NULL",
                                params![old_deleted_at, old_modified_at, image_id],
                            )?;
                        }
                        if !redo.is_empty() {
                            append_redo(
                                &transaction,
                                operation_id,
                                &RedoRecord::ImageStates { items: redo },
                            )?;
                        }
                    }
                }
            }
            _ => {
                let ids: Vec<i64> = {
                    let mut statement = transaction.prepare(
                        "SELECT id FROM images WHERE purge_operation_id = ?1 ORDER BY id",
                    )?;
                    statement
                        .query_map([operation_id], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                if !ids.is_empty() {
                    append_redo(&transaction, operation_id, &RedoRecord::PurgeIds { ids })?;
                }
                transaction.execute(
                    "UPDATE images SET purge_operation_id = NULL
                     WHERE purge_operation_id = ?1",
                    [operation_id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    })
}

fn redo_visibility(database: &LibraryDb, operation_id: i64, kind: HistoryKind) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let records = load_redo_records(&transaction, operation_id)?;
        match kind {
            HistoryKind::Trash | HistoryKind::Restore => {
                let mut applied = false;
                for record in records {
                    match record {
                        RedoRecord::ImageStates { items } => {
                            for item in items {
                                transaction.execute(
                                    "UPDATE images
                                     SET deleted_at = ?1, modified_at = ?2
                                     WHERE id = ?3",
                                    params![item.deleted_at, item.modified_at, item.image_id],
                                )?;
                                applied = true;
                            }
                        }
                        _ => {}
                    }
                }
                if !applied {
                    return Err(AppError::message("这条记录缺少重做数据"));
                }
            }
            _ => {
                let mut applied = false;
                for record in records {
                    match record {
                        RedoRecord::PurgeIds { ids } => {
                            for id in ids {
                                transaction.execute(
                                    "UPDATE images SET purge_operation_id = ?1
                                     WHERE id = ?2 AND deleted_at IS NOT NULL
                                       AND purge_operation_id IS NULL",
                                    params![operation_id, id],
                                )?;
                                applied = true;
                            }
                        }
                        _ => {}
                    }
                }
                if !applied {
                    return Err(AppError::message("这条记录缺少重做数据"));
                }
            }
        }
        transaction.commit()?;
        Ok(())
    })
}

/// 文件夹记录：所有逆操作在同一个事务里完成；需要重做的完整行在删除前写入 redo chunk。
fn undo_folder(
    database: &LibraryDb,
    root: &Path,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for chunk in load_chunks(&transaction, operation_id)? {
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
                    folders::undo_create(&transaction, root, &rows)?;
                }
                HistoryKind::FolderRename | HistoryKind::FolderMove => {
                    let change: FolderPathChange = decode_json(&chunk.data)?;
                    folders::undo_path_change(&transaction, root, &change)?;
                }
                HistoryKind::FolderDelete => {
                    let deleted: FolderDeleted = decode_json(&chunk.data)?;
                    folders::undo_delete(&transaction, root, &deleted)?;
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
        transaction.commit()?;
        Ok(())
    })
}

fn folder_rows(connection: &Connection, refs: &[FolderRef]) -> AppResult<Vec<FolderRow>> {
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

fn redo_folder(
    database: &LibraryDb,
    root: &Path,
    operation_id: i64,
    kind: HistoryKind,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let chunks = load_chunks(&transaction, operation_id)?;
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
                folders::redo_create(&transaction, root, &rows)?;
            }
            HistoryKind::FolderRename | HistoryKind::FolderMove => {
                for chunk in chunks
                    .iter()
                    .filter(|chunk| chunk.format == ChunkFormat::Json)
                {
                    let change: FolderPathChange = decode_json(&chunk.data)?;
                    folders::redo_path_change(&transaction, root, &change)?;
                }
            }
            HistoryKind::FolderDelete => {
                let mut found = false;
                for chunk in chunks
                    .iter()
                    .filter(|chunk| chunk.format == ChunkFormat::Json)
                {
                    let deleted: FolderDeleted = decode_json(&chunk.data)?;
                    folders::redo_delete(&transaction, root, &deleted)?;
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
        transaction.commit()?;
        Ok(())
    })
}

/// 图片移动：按当前 folder 幂等判断，文件先移回、再更新行；中途失败会回滚已完成的文件移动。
fn undo_move(database: &LibraryDb, root: &Path, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let paths = LibraryPaths::new(root);
        let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut created_folders: Vec<FolderRef> = Vec::new();
        let mut redo_moves = Vec::new();
        let result = (|| -> AppResult<()> {
            for chunk in load_chunks(&transaction, operation_id)? {
                if chunk.format == ChunkFormat::Json {
                    created_folders.extend(decode_json::<Vec<FolderRef>>(&chunk.data)?);
                    continue;
                }
                if chunk.format != ChunkFormat::Moves {
                    continue;
                }
                for (image_id, from_folder, to_folder, old_modified_at) in
                    decode_moves(&chunk.data)?
                {
                    let current: Option<(i64, String, String, String, i64)> = transaction
                        .query_row(
                            "SELECT images.folder_id, images.content_hash, images.file_ext,
                                    folders.path, images.modified_at
                             FROM images JOIN folders ON folders.id = images.folder_id
                             WHERE images.id = ?1",
                            [image_id],
                            |row| {
                                Ok((
                                    row.get(0)?,
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                ))
                            },
                        )
                        .optional()?;
                    let Some((folder_id, content_hash, file_ext, to_path, current_modified_at)) =
                        current
                    else {
                        return Err(AppError::message(format!("图片 {image_id} 已不存在")));
                    };
                    if folder_id == from_folder {
                        continue;
                    }
                    if folder_id != to_folder {
                        return Err(AppError::message(format!(
                            "图片 {image_id} 已被移动到其他文件夹"
                        )));
                    }
                    redo_moves.push(MoveState {
                        image_id,
                        from_folder_id: from_folder,
                        to_folder_id: to_folder,
                        new_modified_at: current_modified_at,
                    });
                    let from_path: String = transaction.query_row(
                        "SELECT path FROM folders WHERE id = ?1",
                        [from_folder],
                        |row| row.get(0),
                    )?;
                    let source =
                        metadata::content_path(&paths, &to_path, &content_hash, &file_ext)?;
                    let destination =
                        metadata::content_path(&paths, &from_path, &content_hash, &file_ext)?;
                    if destination.is_file() {
                        if source.is_file() {
                            // 源文件已在原位置（例如上次中断），删掉目标位置的副本即可。
                            let _ = fs::remove_file(&source);
                        }
                    } else {
                        if let Some(parent) = destination.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::rename(&source, &destination)?;
                        renames.push((destination, source));
                    }
                    let changed = transaction.execute(
                        "UPDATE images SET folder_id = ?1, modified_at = ?2 WHERE id = ?3",
                        params![from_folder, old_modified_at, image_id],
                    )?;
                    if changed == 0 {
                        return Err(AppError::message(format!("图片 {image_id} 已不存在")));
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            for (new_path, old_path) in renames.iter().rev() {
                if new_path.is_file() && !old_path.exists() {
                    let _ = fs::rename(new_path, old_path);
                }
            }
            return Err(error);
        }
        if !redo_moves.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::MoveStates { items: redo_moves },
            )?;
        }
        let folder_rows = folder_rows(&transaction, &created_folders)?;
        if !folder_rows.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::FolderRows { items: folder_rows },
            )?;
        }
        folders::cleanup_created(&transaction, root, &created_folders)?;
        transaction.commit()?;
        Ok(())
    })
}

fn redo_move(database: &LibraryDb, root: &Path, operation_id: i64) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let paths = LibraryPaths::new(root);
        let mut moves = Vec::new();
        let mut folders = Vec::new();
        for record in load_redo_records(&transaction, operation_id)? {
            match record {
                RedoRecord::MoveStates { items } => moves.extend(items),
                RedoRecord::FolderRows { items } => folders.extend(items),
                _ => {}
            }
        }
        if moves.is_empty() {
            return Err(AppError::message("这条记录缺少重做数据"));
        }
        folders::redo_create(&transaction, root, &folders)?;

        let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
        let result = (|| -> AppResult<()> {
            for item in &moves {
                let current: Option<(i64, String, String, String)> = transaction
                    .query_row(
                        "SELECT images.folder_id, images.content_hash, images.file_ext,
                                folders.path
                         FROM images JOIN folders ON folders.id = images.folder_id
                         WHERE images.id = ?1",
                        [item.image_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                let Some((folder_id, content_hash, file_ext, from_path)) = current else {
                    return Err(AppError::message(format!(
                        "图片 {} 已不存在",
                        item.image_id
                    )));
                };
                if folder_id == item.to_folder_id {
                    continue;
                }
                if folder_id != item.from_folder_id {
                    return Err(AppError::message(format!(
                        "图片 {} 已被移动到其他文件夹",
                        item.image_id
                    )));
                }
                let to_path: String = transaction.query_row(
                    "SELECT path FROM folders WHERE id = ?1",
                    [item.to_folder_id],
                    |row| row.get(0),
                )?;
                let source = metadata::content_path(&paths, &from_path, &content_hash, &file_ext)?;
                let destination =
                    metadata::content_path(&paths, &to_path, &content_hash, &file_ext)?;
                if destination.is_file() {
                    if source.is_file() {
                        let _ = fs::remove_file(&source);
                    }
                } else {
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::rename(&source, &destination)?;
                    renames.push((destination, source));
                }
                let changed = transaction.execute(
                    "UPDATE images SET folder_id = ?1, modified_at = ?2 WHERE id = ?3",
                    params![item.to_folder_id, item.new_modified_at, item.image_id],
                )?;
                if changed == 0 {
                    return Err(AppError::message(format!(
                        "图片 {} 已不存在",
                        item.image_id
                    )));
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            for (new_path, old_path) in renames.iter().rev() {
                if new_path.is_file() && !old_path.exists() {
                    let _ = fs::rename(new_path, old_path);
                }
            }
            return Err(error);
        }
        transaction.commit()?;
        Ok(())
    })
}

/// 编辑撤销：用暂存旧原图恢复内容列与缩略图；当前内容已经不是本次编辑结果时拒绝猜测。
/// 新原图不删除，移进暂存目录留给重做。
fn undo_edit(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    operation_id: i64,
) -> AppResult<()> {
    let snapshot: EditSnapshot = database.with_reader(|connection| {
        let mut snapshot = None;
        for chunk in load_chunks(connection, operation_id)? {
            if chunk.format == ChunkFormat::Json {
                snapshot = Some(decode_json(&chunk.data)?);
            }
        }
        snapshot.ok_or_else(|| AppError::message("编辑记录缺少内容快照"))
    })?;

    let paths = LibraryPaths::new(root);
    let staged_dir = undo_dir(root, operation_id);
    let old_staged = staged_dir.join(format!("{}.{}", snapshot.content_hash, snapshot.file_ext));
    let new_staged = staged_dir.join(format!(
        "redo-{}.{}",
        snapshot.new_hash, snapshot.new_file_ext
    ));
    let old_content = metadata::content_path(
        &paths,
        &snapshot.folder_path,
        &snapshot.content_hash,
        &snapshot.file_ext,
    )?;
    let new_content = metadata::content_path(
        &paths,
        &snapshot.folder_path,
        &snapshot.new_hash,
        &snapshot.new_file_ext,
    )?;

    let current = content::load(database, snapshot.image_id)?;
    if current.content_hash == snapshot.content_hash {
        // 上次中断已经恢复过内容；尽量把新文件收进暂存，供重做。
        if !new_staged.is_file() && new_content.is_file() {
            if let Err(error) = fs::rename(&new_content, &new_staged) {
                log::warn!(
                    "撤销编辑暂存新原图失败 image={} path={}: {error}",
                    snapshot.image_id,
                    new_content.display()
                );
            }
        } else if new_staged.is_file() && new_content.is_file() {
            let _ = fs::remove_file(&new_content);
        }
        let _ = fs::remove_file(viewer::preview_path(
            &paths,
            snapshot.image_id,
            &snapshot.new_hash,
        ));
        return Ok(());
    }
    if current.content_hash != snapshot.new_hash {
        return Err(AppError::message("图片内容已经变化，无法撤销这次编辑"));
    }

    if !stage_file(&old_content, &old_staged)? {
        return Err(AppError::message("编辑前的原图暂存已丢失，无法撤销"));
    }
    let decoded = decode::decode(&old_staged)?;
    let thumb = thumbnail::encode(&decoded.image)?;
    let location = thumbs.append(&thumb.bytes)?;

    let linked = if old_content.is_file() {
        false
    } else {
        if let Some(parent) = old_content.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::hard_link(&old_staged, &old_content).is_err() {
            fs::copy(&old_staged, &old_content)?;
        }
        true
    };

    let mut staged_new = false;
    if !new_staged.is_file() && new_content.is_file() {
        if let Some(parent) = new_staged.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Err(error) = fs::rename(&new_content, &new_staged) {
            if linked {
                let _ = fs::remove_file(&old_content);
            }
            return Err(error.into());
        }
        staged_new = true;
    }

    let now = now_ms();
    let commit = database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        append_redo(
            &transaction,
            operation_id,
            &RedoRecord::EditState {
                state: EditRedoState {
                    image_id: current.id,
                    folder_path: current.folder_path.clone(),
                    content_hash: current.content_hash.clone(),
                    file_ext: current.file_ext.clone(),
                    file_size: current.file_size,
                    width: current.width,
                    height: current.height,
                    format: current.format.clone(),
                    has_alpha: current.has_alpha,
                    phash: current.phash.clone(),
                    colors4: current.colors4.clone(),
                    colors8: current.colors8.clone(),
                    modified_at: current.modified_at,
                },
            },
        )?;
        let thumb_offset = i64::try_from(location.offset)
            .map_err(|_| AppError::message("缩略图偏移超出数据库范围"))?;
        let changed = transaction.execute(
            "UPDATE images SET
                content_hash = ?1, file_ext = ?2, file_size = ?3, width = ?4, height = ?5,
                format = ?6, has_alpha = ?7, phash = ?8, colors4 = ?9, colors8 = ?10,
                modified_at = ?11, thumb_segment = ?12, thumb_offset = ?13,
                thumb_length = ?14, thumb_format = 'webp', thumb_width = ?15,
                thumb_height = ?16, thumb_version = thumb_version + 1
             WHERE id = ?17",
            params![
                snapshot.content_hash,
                snapshot.file_ext,
                snapshot.file_size,
                snapshot.width,
                snapshot.height,
                snapshot.format,
                snapshot.has_alpha,
                snapshot.phash,
                snapshot.colors4,
                snapshot.colors8,
                snapshot.modified_at,
                location.segment,
                thumb_offset,
                location.length,
                thumb.width,
                thumb.height,
                snapshot.image_id,
            ],
        )?;
        if changed == 0 {
            return Err(AppError::message("图片不存在"));
        }
        transaction.execute(
            "DELETE FROM feature_blobs WHERE image_id = ?1",
            [snapshot.image_id],
        )?;
        repository::ensure_thumb_segment(&transaction, location.segment, now)?;
        if let Ok(size) = thumbs.segment_size_bytes(location.segment) {
            repository::update_thumb_segment_size(&transaction, location.segment, size)?;
        }
        transaction.commit()?;
        Ok(())
    });
    if let Err(error) = commit {
        if staged_new && !new_content.is_file() {
            let _ = fs::rename(&new_staged, &new_content);
        }
        if linked {
            let _ = fs::remove_file(&old_content);
        }
        return Err(error);
    }

    // 行已改回旧哈希；当前内容里的新原图若还在（暂存已有副本），直接删掉。
    if new_content.exists() && new_staged.is_file() {
        if let Err(error) = metadata::remove_content_file(root, &new_content) {
            log::warn!(
                "撤销编辑后清理新原图失败 image={} path={}: {error}",
                snapshot.image_id,
                new_content.display()
            );
        }
    }
    let _ = fs::remove_file(viewer::preview_path(
        &paths,
        snapshot.image_id,
        &snapshot.new_hash,
    ));
    Ok(())
}

fn redo_edit(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    operation_id: i64,
) -> AppResult<()> {
    let snapshot: EditSnapshot = database.with_reader(|connection| {
        let mut snapshot = None;
        for chunk in load_chunks(connection, operation_id)? {
            if chunk.format == ChunkFormat::Json {
                snapshot = Some(decode_json(&chunk.data)?);
            }
        }
        snapshot.ok_or_else(|| AppError::message("编辑记录缺少内容快照"))
    })?;
    let state = database.with_reader(|connection| {
        load_redo_records(connection, operation_id)?
            .into_iter()
            .find_map(|record| match record {
                RedoRecord::EditState { state } => Some(state),
                _ => None,
            })
            .ok_or_else(|| AppError::message("这条记录缺少重做数据"))
    })?;

    let paths = LibraryPaths::new(root);
    let staged_dir = undo_dir(root, operation_id);
    let old_staged = staged_dir.join(format!("{}.{}", snapshot.content_hash, snapshot.file_ext));
    let new_staged = staged_dir.join(format!(
        "redo-{}.{}",
        snapshot.new_hash, snapshot.new_file_ext
    ));
    let old_content = metadata::content_path(
        &paths,
        &snapshot.folder_path,
        &snapshot.content_hash,
        &snapshot.file_ext,
    )?;
    let new_content = metadata::content_path(
        &paths,
        &state.folder_path,
        &state.content_hash,
        &state.file_ext,
    )?;

    let current = content::load(database, snapshot.image_id)?;
    if current.content_hash == state.content_hash {
        // 上次已经重做完；补回撤销所需的旧图暂存，并清掉提交后没来得及清理的副本。
        if stage_file(&old_content, &old_staged)? {
            if new_staged.is_file() && new_content.is_file() {
                let _ = fs::remove_file(&new_staged);
            }
            if old_content != new_content && old_content.exists() {
                let _ = metadata::remove_content_file(root, &old_content);
            }
        } else {
            log::warn!(
                "重做编辑后旧原图暂存缺失 image={} path={}",
                snapshot.image_id,
                old_staged.display()
            );
        }
        let _ = fs::remove_file(viewer::preview_path(
            &paths,
            state.image_id,
            &snapshot.content_hash,
        ));
        return Ok(());
    }
    if current.content_hash != snapshot.content_hash {
        return Err(AppError::message("图片内容已经变化，无法重做这次编辑"));
    }

    if !stage_file(&old_content, &old_staged)? {
        log::warn!(
            "重做编辑前暂存旧原图失败 image={} path={}",
            snapshot.image_id,
            old_content.display()
        );
    }
    let mut moved = false;
    if !new_content.is_file() {
        if !new_staged.is_file() {
            return Err(AppError::message("编辑后的原图暂存已丢失，无法重做"));
        }
        if let Some(parent) = new_content.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&new_staged, &new_content)?;
        moved = true;
    }
    let decoded = decode::decode(&new_content)?;
    let thumb = thumbnail::encode(&decoded.image)?;
    let location = thumbs.append(&thumb.bytes)?;
    let now = now_ms();

    let commit = database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let thumb_offset = i64::try_from(location.offset)
            .map_err(|_| AppError::message("缩略图偏移超出数据库范围"))?;
        let changed = transaction.execute(
            "UPDATE images SET
                content_hash = ?1, file_ext = ?2, file_size = ?3, width = ?4, height = ?5,
                format = ?6, has_alpha = ?7, phash = ?8, colors4 = ?9, colors8 = ?10,
                modified_at = ?11, thumb_segment = ?12, thumb_offset = ?13,
                thumb_length = ?14, thumb_format = 'webp', thumb_width = ?15,
                thumb_height = ?16, thumb_version = thumb_version + 1
             WHERE id = ?17",
            params![
                state.content_hash,
                state.file_ext,
                state.file_size,
                state.width,
                state.height,
                state.format,
                state.has_alpha,
                state.phash,
                state.colors4,
                state.colors8,
                state.modified_at,
                location.segment,
                thumb_offset,
                location.length,
                thumb.width,
                thumb.height,
                state.image_id,
            ],
        )?;
        if changed == 0 {
            return Err(AppError::message("图片不存在"));
        }
        transaction.execute(
            "DELETE FROM feature_blobs WHERE image_id = ?1",
            [state.image_id],
        )?;
        repository::ensure_thumb_segment(&transaction, location.segment, now)?;
        if let Ok(size) = thumbs.segment_size_bytes(location.segment) {
            repository::update_thumb_segment_size(&transaction, location.segment, size)?;
        }
        transaction.commit()?;
        Ok(())
    });
    if let Err(error) = commit {
        if moved && !new_staged.exists() {
            let _ = fs::rename(&new_content, &new_staged);
        }
        return Err(error);
    }

    if new_staged.is_file() {
        let _ = fs::remove_file(&new_staged);
    }
    if old_content != new_content && old_content.exists() {
        if let Err(error) = metadata::remove_content_file(root, &old_content) {
            log::warn!(
                "重做编辑后清理旧原图失败 image={} path={}: {error}",
                state.image_id,
                old_content.display()
            );
        }
    }
    let _ = fs::remove_file(viewer::preview_path(
        &paths,
        state.image_id,
        &snapshot.content_hash,
    ));
    Ok(())
}

/// 把原图放进暂存目录；已有暂存直接返回。目录不存在时创建。
fn stage_file(source: &Path, staged: &Path) -> AppResult<bool> {
    if staged.is_file() {
        return Ok(true);
    }
    if !source.is_file() {
        return Ok(false);
    }
    if let Some(parent) = staged.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::hard_link(source, staged).is_err() {
        fs::copy(source, staged)?;
    }
    Ok(true)
}

/// 导入撤销：暂存原图并记录完整行，删除新增行，恢复合并项快照，再清理本次新建且仍为空/无引用的文件夹与标签。
fn undo_import(
    database: &LibraryDb,
    root: &Path,
    task: &TaskHandle,
    operation_id: i64,
) -> AppResult<()> {
    let chunks = database.with_reader(|connection| load_chunks(connection, operation_id))?;
    let mut created_folders = Vec::new();
    let mut created_tags: Vec<ImportTag> = Vec::new();
    let mut merged: Vec<ImportMerged> = Vec::new();
    for chunk in chunks {
        if chunk.format != ChunkFormat::Json {
            continue;
        }
        match serde_json::from_slice::<ImportRecord>(&chunk.data)? {
            ImportRecord::Folders { items } => created_folders.extend(items),
            ImportRecord::Tags { items } => created_tags.extend(items),
            ImportRecord::Merged { items } => merged.extend(items),
        }
    }

    let mut deleted = 0_usize;
    // 续做时也要知道上一轮已经删掉的原导入行；redo chunks 就是这份清单。
    let mut removed_ids: HashSet<i64> = database.with_reader(|connection| {
        let mut ids = HashSet::new();
        for record in load_redo_records(connection, operation_id)? {
            if let RedoRecord::ImportImages { items } = record {
                ids.extend(items.into_iter().map(|item| item.row.id));
            }
        }
        Ok(ids)
    })?;
    loop {
        let rows = database.with_reader(|connection| load_import_rows(connection, operation_id))?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows.iter().map(|(row, _)| row.id).collect();
        removed_ids.extend(ids.iter().copied());
        let tag_ids = import_tag_relations(database, &ids)?;
        let collection_items = import_collection_relations(database, &ids)?;

        let mut items = Vec::with_capacity(rows.len());
        let mut staged_moves: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (row, folder_path) in rows {
            let tags = tag_ids.get(&row.id).cloned().unwrap_or_default();
            let collections = collection_items.get(&row.id).cloned().unwrap_or_default();
            let (staged_file, moved) = stage_import_file(root, operation_id, &row, &folder_path)?;
            if let Some((source, staged)) = moved {
                staged_moves.push((source, staged));
            }
            items.push(ImportRedoImage {
                row,
                folder_path,
                tag_ids: tags,
                collection_items: collections,
                staged_file,
            });
        }

        let result = database.with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::ImportImages { items },
            )?;
            let marks = crate::library::placeholders(ids.len());
            transaction.execute(
                &format!("DELETE FROM images WHERE id IN ({marks})"),
                rusqlite::params_from_iter(ids.iter()),
            )?;
            transaction.commit()?;
            Ok(())
        });
        if let Err(error) = result {
            // 行还在，把已经搬进暂存的文件放回原位置，保持可重试。
            for (source, staged) in staged_moves.iter().rev() {
                if staged.is_file() && !source.exists() {
                    let _ = fs::rename(staged, source);
                }
            }
            return Err(error);
        }
        deleted += ids.len();
        task.set_message(format!("撤销导入：已移除 {deleted} 张"));
    }

    if !merged.is_empty() {
        restore_import_merges(database, operation_id, &merged, &removed_ids)?;
    }

    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let folder_rows = folder_rows(&transaction, &created_folders)?;
        if !folder_rows.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::FolderRows { items: folder_rows },
            )?;
        }
        if !created_tags.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::ImportTags {
                    items: created_tags
                        .iter()
                        .map(|tag| ImportRedoTag {
                            id: tag.id,
                            name: tag.name.clone(),
                        })
                        .collect(),
                },
            )?;
        }
        folders::cleanup_created(&transaction, root, &created_folders)?;
        for tag in &created_tags {
            let name: Option<String> = transaction
                .query_row("SELECT name FROM tags WHERE id = ?1", [tag.id], |row| {
                    row.get(0)
                })
                .optional()?;
            // 标签 id 也可能被复用；名字对不上就保留，避免删掉别的标签。
            if !name.is_some_and(|name| name.eq_ignore_ascii_case(&tag.name)) {
                continue;
            }
            let used: i64 = transaction.query_row(
                "SELECT count(*) FROM image_tags WHERE tag_id = ?1",
                [tag.id],
                |row| row.get(0),
            )?;
            if used == 0 {
                transaction.execute("DELETE FROM tags WHERE id = ?1", [tag.id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    })
}

fn redo_import(
    database: &LibraryDb,
    root: &Path,
    task: &TaskHandle,
    operation_id: i64,
) -> AppResult<()> {
    let records = database.with_reader(|connection| load_redo_records(connection, operation_id))?;
    let mut images = Vec::new();
    let mut tags = Vec::new();
    let mut folders = Vec::new();
    let mut merged = Vec::new();
    for record in records {
        match record {
            RedoRecord::ImportImages { items } => images.extend(items),
            RedoRecord::ImportTags { items } => tags.extend(items),
            RedoRecord::FolderRows { items } => folders.extend(items),
            RedoRecord::ImportMerged { items } => merged.extend(items),
            _ => {}
        }
    }
    if images.is_empty() && merged.is_empty() && folders.is_empty() && tags.is_empty() {
        return Err(AppError::message("这条记录缺少重做数据"));
    }

    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        folders::redo_create(&transaction, root, &folders)?;
        ensure_import_tags(&transaction, &tags)?;
        transaction.commit()?;
        Ok(())
    })?;

    let total = images.len();
    let mut restored = 0_usize;
    for batch in images.chunks(200) {
        for item in batch {
            restore_import_file(root, operation_id, item)?;
        }
        database.with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for item in batch {
                let existing: Option<(i64, String)> = transaction
                    .query_row(
                        "SELECT id, content_hash FROM images WHERE id = ?1 OR content_hash = ?2",
                        params![item.row.id, item.row.content_hash],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((id, hash)) = existing {
                    if id != item.row.id || hash != item.row.content_hash {
                        return Err(AppError::message(format!(
                            "重做导入时图片 {} 的内容已被占用",
                            item.row.id
                        )));
                    }
                } else {
                    insert_import_row(&transaction, &item.row)?;
                }
                for tag_id in &item.tag_ids {
                    transaction.execute(
                        "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)
                         ON CONFLICT(image_id, tag_id) DO NOTHING",
                        params![item.row.id, tag_id],
                    )?;
                }
                for (collection_id, position) in &item.collection_items {
                    transaction.execute(
                        "INSERT INTO collection_items(collection_id, image_id, position)
                         SELECT ?1, ?2, ?3 WHERE EXISTS (
                             SELECT 1 FROM collections WHERE id = ?1
                         )
                         ON CONFLICT(collection_id, image_id) DO NOTHING",
                        params![collection_id, item.row.id, position],
                    )?;
                }
            }
            transaction.commit()?;
            Ok(())
        })?;
        restored += batch.len();
        task.set_message(format!("重做导入：已恢复 {restored}/{total} 张"));
    }

    if !merged.is_empty() {
        redo_import_merges(database, &merged)?;
    }
    Ok(())
}

fn load_import_rows(
    connection: &Connection,
    operation_id: i64,
) -> AppResult<Vec<(ImportedRow, String)>> {
    let mut statement = connection.prepare(
        "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                images.file_size, images.width, images.height, images.format,
                images.has_alpha, images.phash, images.colors4, images.colors8,
                images.score, images.description, images.description_source,
                images.description_model, images.imported_at, images.modified_at,
                images.deleted_at, images.thumb_segment, images.thumb_offset,
                images.thumb_length, images.thumb_format, images.thumb_width,
                images.thumb_height, images.thumb_version, images.origin_operation_id,
                folders.path
         FROM images JOIN folders ON folders.id = images.folder_id
         WHERE images.origin_operation_id = ?1
         ORDER BY images.id
         LIMIT 400",
    )?;
    let rows = statement
        .query_map([operation_id], |row| {
            Ok((
                ImportedRow {
                    id: row.get(0)?,
                    content_hash: row.get(1)?,
                    file_ext: row.get(2)?,
                    folder_id: row.get(3)?,
                    file_size: row.get(4)?,
                    width: row.get(5)?,
                    height: row.get(6)?,
                    format: row.get(7)?,
                    has_alpha: row.get::<_, i64>(8)? != 0,
                    phash: row.get(9)?,
                    colors4: row.get(10)?,
                    colors8: row.get(11)?,
                    score: row.get(12)?,
                    description: row.get(13)?,
                    description_source: row.get(14)?,
                    description_model: row.get(15)?,
                    imported_at: row.get(16)?,
                    modified_at: row.get(17)?,
                    deleted_at: row.get(18)?,
                    thumb_segment: row.get(19)?,
                    thumb_offset: row.get(20)?,
                    thumb_length: row.get(21)?,
                    thumb_format: row.get(22)?,
                    thumb_width: row.get(23)?,
                    thumb_height: row.get(24)?,
                    thumb_version: row.get(25)?,
                    origin_operation_id: row.get(26)?,
                },
                row.get(27)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn import_tag_relations(database: &LibraryDb, ids: &[i64]) -> AppResult<HashMap<i64, Vec<i64>>> {
    let mut relations: HashMap<i64, Vec<i64>> = HashMap::new();
    if ids.is_empty() {
        return Ok(relations);
    }
    database.with_reader(|connection| {
        let marks = crate::library::placeholders(ids.len());
        let mut statement = connection.prepare(&format!(
            "SELECT image_id, tag_id FROM image_tags
             WHERE image_id IN ({marks}) ORDER BY image_id, tag_id"
        ))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (image_id, tag_id) in rows {
            relations.entry(image_id).or_default().push(tag_id);
        }
        Ok(())
    })?;
    Ok(relations)
}

fn import_collection_relations(
    database: &LibraryDb,
    ids: &[i64],
) -> AppResult<HashMap<i64, Vec<(i64, i64)>>> {
    let mut relations: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    if ids.is_empty() {
        return Ok(relations);
    }
    database.with_reader(|connection| {
        let marks = crate::library::placeholders(ids.len());
        let mut statement = connection.prepare(&format!(
            "SELECT image_id, collection_id, position FROM collection_items
             WHERE image_id IN ({marks}) ORDER BY image_id, collection_id"
        ))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (image_id, collection_id, position) in rows {
            relations
                .entry(image_id)
                .or_default()
                .push((collection_id, position));
        }
        Ok(())
    })?;
    Ok(relations)
}

fn stage_import_file(
    root: &Path,
    operation_id: i64,
    row: &ImportedRow,
    folder_path: &str,
) -> AppResult<(Option<String>, Option<(PathBuf, PathBuf)>)> {
    let name = format!("import-{}.{}", row.id, row.file_ext);
    let staged = undo_dir(root, operation_id).join(&name);
    if staged.is_file() {
        return Ok((Some(name), None));
    }
    let source = metadata::content_path(
        &LibraryPaths::new(root),
        folder_path,
        &row.content_hash,
        &row.file_ext,
    )?;
    if !source.is_file() {
        return Ok((None, None));
    }
    if let Some(parent) = staged.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(&source, &staged) {
        Ok(()) => Ok((Some(name), Some((source, staged)))),
        Err(_) => {
            fs::copy(&source, &staged)?;
            fs::remove_file(&source)?;
            Ok((Some(name), Some((source, staged))))
        }
    }
}

fn restore_import_file(root: &Path, operation_id: i64, item: &ImportRedoImage) -> AppResult<()> {
    let destination = metadata::content_path(
        &LibraryPaths::new(root),
        &item.folder_path,
        &item.row.content_hash,
        &item.row.file_ext,
    )?;
    if destination.is_file() {
        return Ok(());
    }
    let Some(name) = &item.staged_file else {
        return Err(AppError::message(format!(
            "重做导入时图片 {} 的原图文件缺失",
            item.row.id
        )));
    };
    let staged = undo_dir(root, operation_id).join(name);
    if !staged.is_file() {
        return Err(AppError::message(format!(
            "重做导入时图片 {} 的暂存文件缺失",
            item.row.id
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&staged, &destination)?;
    Ok(())
}

fn insert_import_row(transaction: &Transaction<'_>, row: &ImportedRow) -> AppResult<()> {
    transaction.execute(
        "INSERT INTO images(
            id, content_hash, file_ext, folder_id, file_size, width, height, format,
            has_alpha, phash, colors4, colors8, score, description, description_source,
            description_model, imported_at, modified_at, deleted_at, thumb_segment,
            thumb_offset, thumb_length, thumb_format, thumb_width, thumb_height,
            thumb_version, origin_operation_id
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27
         )",
        params![
            row.id,
            row.content_hash,
            row.file_ext,
            row.folder_id,
            row.file_size,
            row.width,
            row.height,
            row.format,
            row.has_alpha,
            row.phash,
            row.colors4,
            row.colors8,
            row.score,
            row.description,
            row.description_source,
            row.description_model,
            row.imported_at,
            row.modified_at,
            row.deleted_at,
            row.thumb_segment,
            row.thumb_offset,
            row.thumb_length,
            row.thumb_format,
            row.thumb_width,
            row.thumb_height,
            row.thumb_version,
            row.origin_operation_id,
        ],
    )?;
    Ok(())
}

fn ensure_import_tags(transaction: &Transaction<'_>, tags: &[ImportRedoTag]) -> AppResult<()> {
    for tag in tags {
        let existing: Option<String> = transaction
            .query_row("SELECT name FROM tags WHERE id = ?1", [tag.id], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(name) = existing {
            if !name.eq_ignore_ascii_case(&tag.name) {
                return Err(AppError::message(format!(
                    "重做导入时标签 id {} 已被其他标签占用",
                    tag.id
                )));
            }
            continue;
        }
        let occupied: Option<i64> = transaction
            .query_row(
                "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
                [&tag.name],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(other) = occupied {
            return Err(AppError::message(format!(
                "重做导入时标签「{}」已被 id {other} 占用",
                tag.name
            )));
        }
        transaction.execute(
            "INSERT INTO tags(id, name) VALUES (?1, ?2)",
            params![tag.id, tag.name],
        )?;
    }
    Ok(())
}

fn restore_import_merges(
    database: &LibraryDb,
    operation_id: i64,
    merged: &[ImportMerged],
    removed_ids: &HashSet<i64>,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut redo = Vec::new();
        for item in merged {
            let current: Option<(i64, i64)> = transaction
                .query_row(
                    "SELECT score, modified_at FROM images WHERE id = ?1",
                    [item.image_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((score, modified_at)) = current else {
                // 本次导入新增、又在同一批里被合并的行已经随导入一起删除。
                if removed_ids.contains(&item.image_id) {
                    continue;
                }
                return Err(AppError::message(format!(
                    "导入合并的图片 {} 已不存在",
                    item.image_id
                )));
            };
            let current_tags = import_tag_ids(&transaction, item.image_id)?;
            let mut old_tags = item.old_tag_ids.clone();
            old_tags.sort_unstable();
            if score == item.old_score
                && modified_at == item.old_modified_at
                && current_tags == old_tags
            {
                continue;
            }
            redo.push(ImportRedoMerged {
                image_id: item.image_id,
                score,
                modified_at,
                tag_ids: current_tags,
            });
            transaction.execute(
                "UPDATE images SET score = ?1, modified_at = ?2 WHERE id = ?3",
                params![item.old_score, item.old_modified_at, item.image_id],
            )?;
            transaction.execute(
                "DELETE FROM image_tags WHERE image_id = ?1",
                [item.image_id],
            )?;
            for tag_id in &item.old_tag_ids {
                transaction.execute(
                    "INSERT INTO image_tags(image_id, tag_id)
                     SELECT ?1, id FROM tags WHERE id = ?2
                     ON CONFLICT(image_id, tag_id) DO NOTHING",
                    params![item.image_id, tag_id],
                )?;
            }
        }
        if !redo.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::ImportMerged { items: redo },
            )?;
        }
        transaction.commit()?;
        Ok(())
    })
}

fn import_tag_ids(connection: &Connection, image_id: i64) -> AppResult<Vec<i64>> {
    let mut statement =
        connection.prepare("SELECT tag_id FROM image_tags WHERE image_id = ?1 ORDER BY tag_id")?;
    let ids = statement
        .query_map([image_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

fn redo_import_merges(database: &LibraryDb, merged: &[ImportRedoMerged]) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for item in merged {
            let changed = transaction.execute(
                "UPDATE images SET score = ?1, modified_at = ?2 WHERE id = ?3",
                params![item.score, item.modified_at, item.image_id],
            )?;
            if changed == 0 {
                return Err(AppError::message(format!(
                    "重做导入合并时图片 {} 已不存在",
                    item.image_id
                )));
            }
            transaction.execute(
                "DELETE FROM image_tags WHERE image_id = ?1",
                [item.image_id],
            )?;
            for tag_id in &item.tag_ids {
                transaction.execute(
                    "INSERT INTO image_tags(image_id, tag_id)
                     SELECT ?1, id FROM tags WHERE id = ?2
                     ON CONFLICT(image_id, tag_id) DO NOTHING",
                    params![item.image_id, tag_id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    })
}

fn load_chunks(connection: &Connection, operation_id: i64) -> AppResult<Vec<Chunk>> {
    let mut statement = connection.prepare(
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

        /// 插入一张能真正解码的 PNG 行，并写一条 feature_blobs，供编辑撤销测试使用。
        fn insert_editable_image(&self) -> (i64, String, PathBuf) {
            let mut image = image::RgbaImage::new(24, 16);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                *pixel = image::Rgba([(x * 9) as u8, (y * 13) as u8, 70, 255]);
            }
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(image)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let path = LibraryPaths::new(&self.root).content_file("人物", &format!("{hash}.png"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
            let image_id = self
                .session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, 'png', ?2, ?3, 24, 16, 'png', 1,
                            zeroblob(32), zeroblob(16), zeroblob(32), 4, '保留描述', 1, 1)",
                        params![hash, self.folder, bytes.len() as i64],
                    )?;
                    let id = connection.last_insert_rowid();
                    connection.execute(
                        "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name,
                            pack_offset, pack_length, updated_at)
                         VALUES (?1, 'embedding', 'm', 3, 'f.bin', 0, 12, 1)",
                        [id],
                    )?;
                    Ok(id)
                })
                .unwrap();
            (image_id, hash, path)
        }
    }

    fn sample_png(path: &Path, seed: u32) {
        let mut image = image::RgbImage::new(48, 32);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                ((x * 3 + seed * 11) % 256) as u8,
                ((y * 5 + seed * 7) % 256) as u8,
                (seed * 37 % 256) as u8,
            ]);
        }
        image.save(path).unwrap();
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

        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entries[1].id,
            task(),
        )
        .unwrap();
        let deleted_states = |fixture: &Fixture| {
            fixture
                .db()
                .with_reader(|connection| {
                    let mut statement =
                        connection.prepare("SELECT id, deleted_at FROM images ORDER BY id")?;
                    let rows = statement
                        .query_map([], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .unwrap()
        };
        assert_eq!(
            deleted_states(&fixture),
            vec![(first, None), (second, None)]
        );
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| entry.status == OperationStatus::Undone)
        );

        run_redo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entries[0].id,
            task(),
        )
        .unwrap();
        let rows = deleted_states(&fixture);
        assert!(rows[0].1.is_none());
        assert!(rows[1].1.is_some());
        assert!(
            list(fixture.db())
                .unwrap()
                .iter()
                .all(|entry| entry.status == OperationStatus::Applied)
        );
    }

    #[test]
    fn permanent_delete_keeps_rows_until_release_and_undo_clears_marker() {
        let fixture = Fixture::new();
        let image_id = fixture.insert_image("a");
        let path = fixture.write_content("a");
        metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();

        let report = metadata::delete_images(fixture.db(), &[image_id], &NoopReporter).unwrap();
        assert!(report.deferred);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.files_removed, 0);
        assert!(path.exists());

        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries[0].kind, HistoryKind::PermanentDelete);

        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entries[0].id,
            task(),
        )
        .unwrap();
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
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries.len(), 2);
        let deleted = entries
            .iter()
            .find(|entry| entry.kind == HistoryKind::PermanentDelete)
            .unwrap();
        assert_eq!(deleted.status, OperationStatus::Undone);

        run_redo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            deleted.id,
            task(),
        )
        .unwrap();
        let purge: Option<i64> = fixture
            .db()
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT purge_operation_id FROM images WHERE id = ?1",
                    [image_id],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(purge, Some(deleted.id));
        assert!(path.exists());
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
        append_live_json(
            fixture.db(),
            pending_content,
            &ImportRecord::Merged {
                items: vec![ImportMerged {
                    image_id: 1,
                    old_score: 0,
                    old_modified_at: 1,
                    old_tag_ids: Vec::new(),
                }],
            },
        )
        .unwrap();
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
    fn redo_to_older_record_leaves_newer_undone() {
        let fixture = Fixture::new();
        let first = fixture.insert_image("a");
        let second = fixture.insert_image("b");
        metadata::set_trashed(fixture.db(), &[first, second], true).unwrap();
        metadata::set_trashed(fixture.db(), &[first], false).unwrap();
        let entries = list(fixture.db()).unwrap();
        let trash = entries
            .iter()
            .find(|entry| entry.kind == HistoryKind::Trash)
            .unwrap()
            .id;
        let restore = entries
            .iter()
            .find(|entry| entry.kind == HistoryKind::Restore)
            .unwrap()
            .id;

        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            trash,
            task(),
        )
        .unwrap();
        run_redo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            trash,
            task(),
        )
        .unwrap();
        let entries = list(fixture.db()).unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == trash)
                .unwrap()
                .status,
            OperationStatus::Applied
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == restore)
                .unwrap()
                .status,
            OperationStatus::Undone
        );
        let trashed: Vec<Option<i64>> = fixture
            .db()
            .with_reader(|connection| {
                let mut statement =
                    connection.prepare("SELECT deleted_at FROM images ORDER BY id")?;
                let rows = statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert!(trashed.iter().all(Option::is_some));

        run_redo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            restore,
            task(),
        )
        .unwrap();
        assert_eq!(
            metadata::image_details(fixture.db(), &fixture.root, first)
                .unwrap()
                .deleted_at,
            None
        );
    }

    #[test]
    fn new_operation_discards_undone_branch() {
        let fixture = Fixture::new();
        let image_id = fixture.insert_image("a");
        metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
        let entry = list(fixture.db()).unwrap().remove(0);
        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entry.id,
            task(),
        )
        .unwrap();
        assert_eq!(
            list(fixture.db()).unwrap()[0].status,
            OperationStatus::Undone
        );

        metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, OperationStatus::Applied);
        assert_eq!(entries[0].id, entry.id + 1);
    }

    #[test]
    fn recovers_interrupted_redo() {
        let fixture = Fixture::new();
        let image_id = fixture.insert_image("a");
        metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
        let entry = list(fixture.db()).unwrap().remove(0);
        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entry.id,
            task(),
        )
        .unwrap();
        fixture
            .db()
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE operations SET status = 'redoing' WHERE id = ?1",
                    [entry.id],
                )?;
                Ok(())
            })
            .unwrap();

        let plan = recover_on_open(fixture.db()).unwrap();
        assert_eq!(plan.resume_redo, Some(entry.id));
        run_redo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entry.id,
            task(),
        )
        .unwrap();
        assert_eq!(
            list(fixture.db()).unwrap()[0].status,
            OperationStatus::Applied
        );
        assert!(
            metadata::image_details(fixture.db(), &fixture.root, image_id)
                .unwrap()
                .deleted_at
                .is_some()
        );
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
        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entries[0].id,
            handle,
        )
        .unwrap();
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

    #[test]
    fn undoes_moves_and_restores_file_location() {
        let fixture = Fixture::new();
        let image_id = fixture.insert_image("a");
        let old_path = fixture.write_content("a");
        let report = metadata::move_images(
            fixture.db(),
            &fixture.root,
            &[image_id],
            "风景",
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.moved, 1);
        let moved_path = LibraryPaths::new(&fixture.root)
            .content_file("风景", &format!("{}.jpg", "0".repeat(63) + "a"));
        assert!(moved_path.exists());
        assert!(!old_path.exists());

        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries[0].kind, HistoryKind::Move);
        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entries[0].id,
            task(),
        )
        .unwrap();

        assert_eq!(
            metadata::image_details(fixture.db(), &fixture.root, image_id)
                .unwrap()
                .folder_path,
            "人物"
        );
        assert!(old_path.exists());
        assert!(!moved_path.exists());
        assert!(!fixture.root.join("风景").exists());
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries[0].status, OperationStatus::Undone);

        run_redo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entries[0].id,
            task(),
        )
        .unwrap();
        assert_eq!(
            metadata::image_details(fixture.db(), &fixture.root, image_id)
                .unwrap()
                .folder_path,
            "风景"
        );
        assert!(!old_path.exists());
        assert!(moved_path.exists());
        assert!(fixture.root.join("风景").is_dir());
        assert_eq!(
            list(fixture.db()).unwrap()[0].status,
            OperationStatus::Applied
        );
    }

    #[test]
    fn undo_move_is_idempotent_after_partial_restore() {
        let fixture = Fixture::new();
        let image_id = fixture.insert_image("a");
        let old_path = fixture.write_content("a");
        metadata::move_images(
            fixture.db(),
            &fixture.root,
            &[image_id],
            "风景",
            &NoopReporter,
        )
        .unwrap();
        let entry = list(fixture.db()).unwrap().remove(0);

        // 模拟“文件已移回、行已更新，但进程在删记录前退出”。
        let moved_path = LibraryPaths::new(&fixture.root)
            .content_file("风景", &format!("{}.jpg", "0".repeat(63) + "a"));
        fs::rename(&moved_path, &old_path).unwrap();
        fixture
            .db()
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE images SET folder_id = ?1 WHERE id = ?2",
                    params![fixture.folder, image_id],
                )?;
                connection.execute(
                    "UPDATE operations SET status = 'undoing' WHERE id = ?1",
                    [entry.id],
                )?;
                Ok(())
            })
            .unwrap();

        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entry.id,
            task(),
        )
        .unwrap();
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries[0].status, OperationStatus::Undone);
        assert!(old_path.exists());
        assert!(!fixture.root.join("风景").exists());
    }

    #[test]
    fn undoes_edit_restoring_file_row_thumbnail_and_features() {
        use crate::library::edit::{self, EditRequest};

        let fixture = Fixture::new();
        let (image_id, old_hash, old_path) = fixture.insert_editable_image();
        let request = EditRequest {
            image_id,
            rotate: 1,
            flip_horizontal: false,
            flip_vertical: false,
            crop: None,
            resize: None,
            output_format: None,
            quality: Some(90),
        };
        let result = edit::save(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            &request,
        )
        .unwrap();
        assert!(result.changed);
        assert!(!old_path.exists());
        let edited_path = LibraryPaths::new(&fixture.root)
            .content_file("人物", &format!("{}.png", result.content_hash));
        assert!(edited_path.exists());

        let entry = list(fixture.db())
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == HistoryKind::Edit)
            .unwrap();
        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entry.id,
            task(),
        )
        .unwrap();

        let details = metadata::image_details(fixture.db(), &fixture.root, image_id).unwrap();
        assert_eq!(details.content_hash, old_hash);
        assert_eq!((details.width, details.height), (24, 16));
        assert!(old_path.exists());
        assert!(!edited_path.exists());
        let features: i64 = fixture
            .db()
            .with_reader(|connection| {
                Ok(connection
                    .query_row("SELECT count(*) FROM feature_blobs", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(features, 0);
        let pointer = crate::library::thumbs::load_pointer(fixture.db(), image_id)
            .unwrap()
            .unwrap();
        assert_eq!(pointer.version, 3);
        assert!(fixture.session.thumbs.read(pointer.location).is_ok());
        assert!(undo_dir(&fixture.root, entry.id).exists());
        assert_eq!(
            list(fixture.db()).unwrap()[0].status,
            OperationStatus::Undone
        );

        run_redo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            entry.id,
            task(),
        )
        .unwrap();
        let details = metadata::image_details(fixture.db(), &fixture.root, image_id).unwrap();
        assert_eq!(details.content_hash, result.content_hash);
        assert_eq!((details.width, details.height), (16, 24));
        assert!(edited_path.exists());
        assert!(!old_path.exists());
        assert_eq!(
            list(fixture.db()).unwrap()[0].status,
            OperationStatus::Applied
        );
        let pointer = crate::library::thumbs::load_pointer(fixture.db(), image_id)
            .unwrap()
            .unwrap();
        assert_eq!(pointer.version, 4);
        assert!(fixture.session.thumbs.read(pointer.location).is_ok());
    }

    #[test]
    fn undoes_import_new_rows_and_merges() {
        use crate::import::{
            FolderMode, ImportRequest, ImportSource,
            job::{DuplicatePolicy, ImportJob},
            pipeline,
        };

        let fixture = Fixture::new();
        let source_dir = fixture._temp.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        sample_png(&source_dir.join("a.png"), 1);
        sample_png(&source_dir.join("b.png"), 2);

        let request = |merge: bool, tag: &str, score: u8| ImportRequest {
            paths: vec![source_dir.to_string_lossy().into_owned()],
            folder: "导入".into(),
            folder_mode: FolderMode::Flat,
            tags: vec![tag.to_owned()],
            score,
            duplicate_policy: if merge {
                DuplicatePolicy::Merge
            } else {
                DuplicatePolicy::Skip
            },
            perceptual_threshold: 0,
            worker_threads: Some(2),
            #[cfg(test)]
            cancel_after: None,
        };
        let run_import = |operation_id: i64, merge: bool, tag: &str, score: u8| {
            let source = ImportSource {
                database: fixture.session.db.clone(),
                thumbs: fixture.session.thumbs.clone(),
                root: fixture.root.clone(),
                operation_id: Some(operation_id),
            };
            let job = Arc::new(ImportJob::new("import-history", "测试"));
            pipeline::run(source, job, request(merge, tag, score)).unwrap();
            finish_live(fixture.db(), operation_id, "").unwrap();
        };

        let first = begin_live(
            fixture.db(),
            NewOperation::new(HistoryKind::Import, "第一次导入"),
        )
        .unwrap();
        run_import(first, false, "初始", 2);
        let second = begin_live(
            fixture.db(),
            NewOperation::new(HistoryKind::Import, "第二次导入"),
        )
        .unwrap();
        run_import(second, true, "追加", 5);

        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].affected, 2);

        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            second,
            task(),
        )
        .unwrap();
        let (score, names): (i64, Vec<String>) = fixture
            .db()
            .with_reader(|connection| {
                let score =
                    connection
                        .query_row("SELECT score FROM images LIMIT 1", [], |row| row.get(0))?;
                let mut statement = connection.prepare(
                    "SELECT tags.name FROM image_tags JOIN tags ON tags.id = image_tags.tag_id
                     ORDER BY tags.name",
                )?;
                let names = statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok((score, names))
            })
            .unwrap();
        assert_eq!(score, 2);
        assert_eq!(names, vec!["初始".to_owned(), "初始".to_owned()]);
        let extra_tag: i64 = fixture
            .db()
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM tags WHERE name = '追加'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(extra_tag, 0);
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries[0].id, second);
        assert_eq!(entries[0].status, OperationStatus::Undone);

        run_redo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            second,
            task(),
        )
        .unwrap();
        let (images, score, names): (i64, i64, Vec<String>) = fixture
            .db()
            .with_reader(|connection| {
                let images =
                    connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?;
                let score =
                    connection
                        .query_row("SELECT score FROM images LIMIT 1", [], |row| row.get(0))?;
                let mut statement = connection.prepare(
                    "SELECT tags.name FROM image_tags JOIN tags ON tags.id = image_tags.tag_id
                     ORDER BY tags.name",
                )?;
                let names = statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok((images, score, names))
            })
            .unwrap();
        assert_eq!(images, 2);
        assert_eq!(score, 5);
        assert_eq!(
            names,
            vec![
                "初始".to_owned(),
                "初始".to_owned(),
                "追加".to_owned(),
                "追加".to_owned()
            ]
        );
        assert!(
            list(fixture.db())
                .unwrap()
                .iter()
                .all(|entry| entry.status == OperationStatus::Applied)
        );

        run_undo(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            first,
            task(),
        )
        .unwrap();
        let remaining: (i64, i64) = fixture
            .db()
            .with_reader(|connection| {
                Ok((
                    connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?,
                    connection.query_row("SELECT count(*) FROM tags", [], |row| row.get(0))?,
                ))
            })
            .unwrap();
        assert_eq!(remaining, (0, 0));
        assert!(!fixture.root.join("导入").exists());
        let entries = list(fixture.db()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| entry.status == OperationStatus::Undone)
        );
    }
}

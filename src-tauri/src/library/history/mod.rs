//! 图库操作记录、撤销与重做。
//!
//! 每条操作记录与产生它的写操作处在同一个 SQLite 事务里；批量操作只记一条。
//! 撤销按记录 id 线性回退：撤销目标及其之后仍为 `applied` 的记录，最新的先撤。
//! 撤销成功的记录不删除，而是标为 `undone` 留在列表里；重做可以从当前状态回到其中一条。
//! 永久删除与清空回收站只写 `purge_operation_id` 标记，记录过期或清空记录后才真正删行删文件。

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::{Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::{db::LibraryDb, fs_journal::FileJournal, paths::LibraryPaths, service::now_ms},
};

mod chunks;
mod replay;
mod undo {
    pub(super) mod collections;
    pub(super) mod edit;
    pub(super) mod folders;
    pub(super) mod import;
    pub(super) mod metadata;
    pub(super) mod r#move;
    pub(super) mod tags;
    pub(super) mod visibility;
}

use chunks::insert_chunks;

pub use replay::{
    cleanup_undo_dirs, clear, has_interrupted_run, has_orphan_purge, list, pending_summary,
    preview_redo, preview_undo, recover_on_open, retry_undo, run_redo, run_release, run_undo,
    skip_undo, storage_usage, trim,
};
pub(crate) use undo::collections::collection_snapshot;

#[cfg(test)]
mod tests;

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
    /// 整棵子树移入回收站；可恢复。
    FolderTrash,
    /// 从回收站恢复文件夹；可撤销。
    FolderRestore,
    /// 永久删除回收站文件夹；只写待释放标记，清空记录后不可撤销。
    FolderPurge,
    Score,
    Description,
    TagSet,
    TagAdd,
    TagRemove,
    TagRename,
    TagDelete,
    /// 把源标签的全部关系迁移到目标标签后删除源标签行。
    TagMerge,
    CollectionCreate,
    CollectionRename,
    CollectionDelete,
    CollectionReorder,
    CollectionAdd,
    CollectionRemove,
    CollectionCombine,
    SimilarityGroups,
    AiDescription,
    /// 图片空间压缩；文件已替换、原图已删除，不可撤销。
    Optimize,
    /// 把检查报告里的孤儿文件移入 `recycle/orphans/`；不涉及数据库行。
    OrphanMove,
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
            Self::FolderTrash => "folderTrash",
            Self::FolderRestore => "folderRestore",
            Self::FolderPurge => "folderPurge",
            Self::Score => "score",
            Self::Description => "description",
            Self::TagSet => "tagSet",
            Self::TagAdd => "tagAdd",
            Self::TagRemove => "tagRemove",
            Self::TagRename => "tagRename",
            Self::TagDelete => "tagDelete",
            Self::TagMerge => "tagMerge",
            Self::CollectionCreate => "collectionCreate",
            Self::CollectionRename => "collectionRename",
            Self::CollectionDelete => "collectionDelete",
            Self::CollectionReorder => "collectionReorder",
            Self::CollectionAdd => "collectionAdd",
            Self::CollectionRemove => "collectionRemove",
            Self::CollectionCombine => "collectionCombine",
            Self::SimilarityGroups => "similarityGroups",
            Self::AiDescription => "aiDescription",
            Self::Optimize => "optimize",
            Self::OrphanMove => "orphanMove",
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
            "folderTrash" => Self::FolderTrash,
            "folderRestore" => Self::FolderRestore,
            "folderPurge" => Self::FolderPurge,
            "score" => Self::Score,
            "description" => Self::Description,
            "tagSet" => Self::TagSet,
            "tagAdd" => Self::TagAdd,
            "tagRemove" => Self::TagRemove,
            "tagRename" => Self::TagRename,
            "tagDelete" => Self::TagDelete,
            "tagMerge" => Self::TagMerge,
            "collectionCreate" => Self::CollectionCreate,
            "collectionRename" => Self::CollectionRename,
            "collectionDelete" => Self::CollectionDelete,
            "collectionReorder" => Self::CollectionReorder,
            "collectionAdd" => Self::CollectionAdd,
            "collectionRemove" => Self::CollectionRemove,
            "collectionCombine" => Self::CollectionCombine,
            "similarityGroups" => Self::SimilarityGroups,
            "aiDescription" => Self::AiDescription,
            "optimize" => Self::Optimize,
            "orphanMove" => Self::OrphanMove,
            other => {
                return Err(AppError::message(format!("未知的操作类型：{other}")));
            }
        };
        Ok(kind)
    }

    /// 记录被裁掉时，这次操作标记的待删除图片失去撤销可能，要安排释放文件。
    pub fn releases_files(self) -> bool {
        matches!(
            self,
            Self::PermanentDelete | Self::EmptyTrash | Self::FolderPurge
        )
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
    /// 用户放弃撤销失败的记录：保留在时间线里，不再参与撤销与重做。
    UndoSkipped,
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
            Self::UndoSkipped => "undoSkipped",
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
            "undoSkipped" => Ok(Self::UndoSkipped),
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
    /// 文件夹批量移动时目标父级兄弟的排序位变化（id、旧值、新值）。
    SortJson,
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
            Self::SortJson => "sortJson",
            Self::Redo => "redo",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "ids" => Ok(Self::Ids),
            "triples" => Ok(Self::Triples),
            "moves" => Ok(Self::Moves),
            "json" => Ok(Self::Json),
            "sortJson" => Ok(Self::SortJson),
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

/// 一次写事务的草稿：逆数据、影响行数与文件补偿日志。
///
/// `fs` 由 [`write_with_id_mode`] 提供并在事务失败时统一撤回，写闭包只负责
/// “做了什么就记什么”，不再各自写回滚分支。
pub struct OperationDraft<'a> {
    id: i64,
    affected: u64,
    chunks: Vec<Chunk>,
    fs: &'a mut FileJournal,
}

impl OperationDraft<'_> {
    pub fn id(&self) -> i64 {
        self.id
    }

    /// 文件补偿日志：搬移、发布、建/删目录都记在这里。
    pub fn journal(&mut self) -> &mut FileJournal {
        self.fs
    }

    pub fn affected(&mut self, n: u64) {
        self.affected = n;
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

    pub fn sort_changes(&mut self, rows: &FolderSortChange) -> AppResult<()> {
        if rows.rows.is_empty() {
            return Ok(());
        }
        self.chunks.push(Chunk {
            format: ChunkFormat::SortJson,
            data: serde_json::to_vec(rows)?,
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

/// 记录格式：批量移动时目标父级非移动兄弟的排序位（id、旧值、新值）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSortChange {
    pub rows: Vec<(i64, i64, i64)>,
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
    ImageStates {
        items: Vec<ImageState>,
    },
    PurgeIds {
        ids: Vec<i64>,
    },
    PurgeFolderIds {
        ids: Vec<i64>,
    },
    MoveStates {
        items: Vec<MoveState>,
    },
    FolderRows {
        items: Vec<FolderRow>,
    },
    FolderSort {
        rows: Vec<(i64, i64)>,
    },
    EditState {
        state: EditRedoState,
    },
    ImportImages {
        items: Vec<ImportRedoImage>,
    },
    ImportTags {
        items: Vec<ImportRedoTag>,
    },
    ImportMerged {
        items: Vec<ImportRedoMerged>,
    },
    ScoreStates {
        items: Vec<ScoreState>,
    },
    DescriptionStates {
        items: Vec<DescriptionRecord>,
    },
    TagNames {
        items: Vec<TagRefRecord>,
    },
    TagSets {
        items: Vec<TagSetState>,
    },
    TagAdds {
        pairs: Vec<(i64, i64)>,
    },
    TagRemovals {
        pairs: Vec<(i64, i64)>,
    },
    TagDeletions {
        ids: Vec<i64>,
    },
    CollectionRestores {
        items: Vec<CollectionSnapshot>,
    },
    CollectionDeletions {
        ids: Vec<i64>,
    },
    CollectionNames {
        items: Vec<CollectionNameState>,
    },
    CollectionOrders {
        rows: Vec<CollectionOrderState>,
    },
    CollectionAdds {
        collection_id: i64,
        items: Vec<(i64, i64)>,
        updated_at: i64,
    },
    CollectionRemovals {
        collection_id: i64,
        items: Vec<(i64, i64)>,
        updated_at: i64,
    },
    /// 孤儿清理：`(来源绝对路径, 回收站绝对路径)`；撤销按它反向搬回，重做正向搬走。
    PathMoves {
        pairs: Vec<(String, String)>,
    },
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

/// 评分撤销/重做状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoreState {
    pub image_id: i64,
    pub score: i64,
    pub modified_at: i64,
}

/// 描述撤销/重做状态；原始记录里是旧值，`redo` 里是撤销前的新值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescriptionRecord {
    pub image_id: i64,
    pub description: String,
    pub description_source: String,
    pub description_model: String,
    pub modified_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagRefRecord {
    pub id: i64,
    pub name: String,
}

/// 标签写入的逆数据；批量关系按批追加，避免一次性 JSON 过大。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TagRecord {
    Set {
        image_id: i64,
        old_tag_ids: Vec<i64>,
        created: Vec<TagRefRecord>,
    },
    Add {
        pairs: Vec<(i64, i64)>,
        created: Vec<TagRefRecord>,
    },
    Remove {
        pairs: Vec<(i64, i64)>,
    },
    Rename {
        tag_id: i64,
        old_name: String,
    },
    Delete {
        tag_id: i64,
        name: String,
    },
    /// 合并的表头：源标签被删、关系迁到目标标签；关系清单见 `Pairs` 与 `MergeAdded`。
    Merge {
        source_id: i64,
        source_name: String,
        target_id: i64,
    },
    /// 合并时新写到目标标签上的图片 id；撤销要按它把目标恢复原样。
    MergeAdded {
        image_ids: Vec<i64>,
    },
    Pairs {
        pairs: Vec<(i64, i64)>,
    },
}

/// 标签集合的完整状态；重做时先清空图片关系再插回。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagSetState {
    pub image_id: i64,
    pub tag_ids: Vec<i64>,
    pub tags: Vec<TagRefRecord>,
}

/// 集合的完整快照；创建、删除、运算与相似分组共用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionSnapshot {
    pub id: i64,
    pub name: String,
    pub sort_order: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// `(imageId, position)`，按 `position` 升序。
    pub items: Vec<(i64, i64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionNameState {
    pub id: i64,
    pub name: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionOrderState {
    pub id: i64,
    pub sort_order: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionRenameRecord {
    pub id: i64,
    pub old_name: String,
    pub old_updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionReorderRecord {
    pub rows: Vec<CollectionOrderState>,
}

/// 集合加入/移出的逆数据，记下操作前的 `updated_at`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItemsRecord {
    pub collection_id: i64,
    pub items: Vec<(i64, i64)>,
    pub old_updated_at: i64,
}

/// 同步写操作：`work` 与记录写入处在同一个事务里。
/// `affected == 0` 且没有逆数据时整条回滚：既不留空记录，也不清空重做分支。
///
/// 写闭包里的文件动作通过 `draft.journal()` 记账；业务错误、SQL 失败、提交失败与
/// panic 展开都会在文件补偿日志析构时逆序撤回，提交成功才保留这些动作。
pub fn write<T>(
    database: &LibraryDb,
    op: NewOperation<'_>,
    work: impl FnOnce(&Transaction<'_>, &mut OperationDraft<'_>) -> AppResult<T>,
) -> AppResult<T> {
    write_with_id(database, op, work).map(|(value, _)| value)
}

/// 与 [`write`] 相同，但返回新建记录的 id；没有产生记录（空操作）时为 `None`。
/// MCP 写入用它在工具结果里给出 `operationId`。
pub fn write_with_id<T>(
    database: &LibraryDb,
    op: NewOperation<'_>,
    work: impl FnOnce(&Transaction<'_>, &mut OperationDraft<'_>) -> AppResult<T>,
) -> AppResult<(T, Option<i64>)> {
    write_with_id_mode(database, op, true, work)
}

/// 只在 `record = false` 时使用的无记录事务：临时图片的元数据修改用它，
/// 既不写 operations，也不清空重做分支。`draft` 只是为了让工作闭包保持同一签名。
pub fn write_with_id_mode<T>(
    database: &LibraryDb,
    op: NewOperation<'_>,
    record: bool,
    work: impl FnOnce(&Transaction<'_>, &mut OperationDraft<'_>) -> AppResult<T>,
) -> AppResult<(T, Option<i64>)> {
    // 装填的补偿日志：只有提交成功才 `disarm()`，其余退出路径（含 panic）都会撤回。
    let mut journal = FileJournal::armed();
    let outcome = database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !record {
            let mut draft = OperationDraft {
                id: 0,
                affected: 0,
                chunks: Vec::new(),
                fs: &mut journal,
            };
            let value = work(&transaction, &mut draft)?;
            transaction.commit()?;
            return Ok((value, None));
        }
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
            fs: &mut journal,
        };
        let value = work(&transaction, &mut draft)?;
        if draft.affected == 0 && draft.chunks.is_empty() {
            // 事务随 drop 回滚；已经落地的文件动作也必须一起撤回，否则磁盘比库新。
            draft.fs.rollback();
            return Ok((value, None));
        }
        transaction.execute(
            "UPDATE operations SET status = 'applied', affected = ?1 WHERE id = ?2",
            params![draft.affected as i64, draft.id],
        )?;
        insert_chunks(&transaction, draft.id, &draft.chunks)?;
        let operation_id = draft.id;
        transaction.commit()?;
        Ok((value, Some(operation_id)))
    });
    if outcome.is_ok() {
        journal.disarm();
    }
    outcome
}

/// 长任务先建 `pending` 记录，再在每批写入时追加逆数据；任务结束后 `finish_live`。
///
/// 这里不结束重做分支：导入可能因为资源冲突根本没启动，或者整批都是重复跳过。
/// 只有真正写入内容的批次（`discard_redo_in_tx`）才算开启新时间线。
pub fn begin_live(database: &LibraryDb, op: NewOperation<'_>) -> AppResult<i64> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
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

/// 删除灰色记录，开启新时间线；由 `write` 与写入内容的导入批次调用。
fn discard_redo_branch(transaction: &Transaction<'_>) -> AppResult<()> {
    transaction.execute(
        "DELETE FROM operations WHERE status IN ('undone', 'redoing')",
        [],
    )?;
    Ok(())
}

/// 长任务在**确实写入内容**的那一批事务里调用，与 `write` 的新时间线语义一致。
pub fn discard_redo_in_tx(tx: &Transaction<'_>) -> AppResult<()> {
    discard_redo_branch(tx)
}

/// 已持有写事务（导入批次、撤销/重做）时使用，避免再次加写锁造成死锁。
///
/// 记录行已经不在了（例如收尾已把空记录删掉）就跳过：逆数据写不进去，
/// 不能让整批已入库的图片因为一个外键失败一起回滚。
pub fn append_live_in_tx(
    tx: &Transaction<'_>,
    op_id: i64,
    format: ChunkFormat,
    data: &[u8],
) -> AppResult<()> {
    let exists: i64 = tx.query_row(
        "SELECT count(*) FROM operations WHERE id = ?1",
        [op_id],
        |row| row.get(0),
    )?;
    if exists == 0 {
        log::warn!("操作记录 {op_id} 已不存在，跳过逆数据写入");
        return Ok(());
    }
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

/// 长任务的 live 记录守卫：正常结束、取消、失败与 panic 都必须收尾。
/// 强杀进程时 Drop 不执行，仍由下次打开图库的 `recover_on_open` 兜底。
pub struct LiveOperation {
    database: Arc<LibraryDb>,
    id: i64,
    finished: bool,
}

impl LiveOperation {
    pub fn begin(database: &Arc<LibraryDb>, op: NewOperation<'_>) -> AppResult<Self> {
        let id = begin_live(database, op)?;
        Ok(Self {
            database: database.clone(),
            id,
            finished: false,
        })
    }

    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn finish(mut self, error: &str) -> AppResult<()> {
        self.finished = true;
        finish_live(&self.database, self.id, error)
    }
}

impl Drop for LiveOperation {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Err(error) = finish_live(&self.database, self.id, "任务异常终止") {
            log::warn!("收尾操作记录 {} 失败：{error}", self.id);
        }
    }
}

/// 长任务完成后把结果写进记录标题（例如“省 12 GB”）；不改表结构。
pub fn update_label(database: &LibraryDb, operation_id: i64, label: &str) -> AppResult<()> {
    database.with_writer(|connection| {
        connection.execute(
            "UPDATE operations SET label = ?1 WHERE id = ?2",
            params![label, operation_id],
        )?;
        Ok(())
    })
}

/// 在记录标题前加来源前缀（MCP 写入用“AI · ”）；不改表结构。
pub fn prefix_label(database: &LibraryDb, operation_id: i64, prefix: &str) -> AppResult<()> {
    database.with_writer(|connection| {
        connection.execute(
            "UPDATE operations SET label = ?1 || label WHERE id = ?2",
            params![prefix, operation_id],
        )?;
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
pub struct HistoryStorage {
    /// 全部操作行数，包含当前任务刚建但还没收尾的 pending 记录。
    pub record_count: u64,
    pub chunk_count: u64,
    pub chunk_bytes: u64,
    pub undo_files: u64,
    pub undo_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub max_steps: u32,
    pub pending_purge_count: u64,
    pub pending_purge_bytes: u64,
    pub storage: HistoryStorage,
    /// 上次撤销/重做异常中断，且此刻没有对应任务在跑。
    pub recovery_needed: bool,
    /// 此刻能否开始撤销或重做；不能时的原因在 `blocked_reason`。
    pub can_run: bool,
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
    /// 崩溃前 `redoing` 的记录里最大的 id，也就是这次重做选中的目标；有值时启动后要续做重做。
    pub resume_redo: Option<i64>,
    /// 是否存在没有记录引用的待释放批次。
    pub release_needed: bool,
}

/// 撤销/重做前的回退清单：会动几条、合计影响数、逐条记录与阻塞原因。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationPreview {
    pub target_id: i64,
    pub steps: u64,
    pub affected: u64,
    pub entries: Vec<HistoryEntry>,
    pub blocked_reason: Option<String>,
}

pub fn undo_dir(root: &Path, operation_id: i64) -> PathBuf {
    LibraryPaths::new(root).undo.join(operation_id.to_string())
}

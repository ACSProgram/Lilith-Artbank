//! 统一的后台任务总线。
//!
//! 所有会长时间占用磁盘、写连接或 AI 服务的操作都注册到这里，准入由
//! `concurrency::resources` 的资源表决定：只有真正读写同一资源的任务才互相等待；
//! 长任务冲突时默认按资源 FIFO 排队，路径锁写者优先，短操作仍立即失败。
//! 任务体 panic、线程起不来或提前返回都在这里落到失败终态，并释放资源。
//! 排队中的任务不占任何资源；真正开始时还要核对图库代次，换库后直接取消。
//! 进度通过 `task-progress` / `task-finished` 事件推给前端任务中心。

use std::{any::Any, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    imageops::optimize::OptimizeOptions,
    library::{
        db::LibraryDb,
        query::{self, LibraryQuery, LibraryView},
    },
};

mod bus;
mod handle;

#[cfg(test)]
mod tests;

pub use bus::TaskBus;
pub use handle::{LaneGuard, TaskHandle};

/// 任务历史保留条数（含已结束），只存在内存里；运行中与排队中的任务不会被裁掉。
const MAX_TASKS: usize = 50;
const MAX_FAILURES: usize = 200;
const EMIT_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskKind {
    Import,
    Thumbnail,
    AiEmbedding,
    AiPose,
    AiDescription,
    SimilarityGroups,
    FolderMove,
    PermanentDelete,
    EmptyTrash,
    Undo,
    Redo,
    /// 只删除操作记录与撤销暂存，不释放待删除文件。
    ClearHistory,
    ReleaseGarbage,
    OptimizeEstimate,
    Optimize,
    /// 仅用于临时区的 embedding/pose 生成；占用 AI 服务资源。
    TempFeatures,
    /// 图库检查：快速检查或完整校验；不持路径锁，容忍扫描期间的变动。
    CheckLibrary,
    /// 导出到目录：复制原图，占根路径读锁。
    Export,
    /// 把检查报告里的孤儿移入回收站；可撤销。
    OrphanMove,
    /// 缩略图包回收；空闲时自动提交，也可以手动「立即回收」。
    PackCompact,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Import => "导入",
            Self::Thumbnail => "重建缩略图",
            Self::AiEmbedding => "生成 embedding",
            Self::AiPose => "生成姿势",
            Self::AiDescription => "生成描述",
            Self::SimilarityGroups => "相似分组",
            Self::FolderMove => "移动图片",
            Self::PermanentDelete => "永久删除",
            Self::EmptyTrash => "清空回收站",
            Self::Undo => "撤销操作",
            Self::Redo => "重做操作",
            Self::ClearHistory => "清空操作记录",
            Self::ReleaseGarbage => "释放删除文件",
            Self::OptimizeEstimate => "估算压缩空间",
            Self::Optimize => "优化图片空间",
            Self::TempFeatures => "生成临时区特征",
            Self::CheckLibrary => "检查图库",
            Self::Export => "导出图片",
            Self::OrphanMove => "清理孤儿文件",
            Self::PackCompact => "回收缩略图包",
        }
    }

    /// 关闭图库时可以直接取消的任务；其余写任务仍要求用户等待完成。
    ///
    /// 释放删除文件也在其中：它按块检查取消，停下时待释放标记还在，
    /// 下次释放接着删，所以关库不必等它把几十万文件删完。
    pub fn can_cancel_for_close(self) -> bool {
        matches!(
            self,
            Self::Thumbnail
                | Self::AiEmbedding
                | Self::AiPose
                | Self::AiDescription
                | Self::SimilarityGroups
                | Self::OptimizeEstimate
                | Self::TempFeatures
                | Self::CheckLibrary
                | Self::Export
                | Self::PackCompact
                | Self::ReleaseGarbage
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskPhase {
    Queued,
    Preparing,
    Running,
    /// 已收到取消请求，正在等当前批次收尾；不是终态。
    Cancelling,
    Completed,
    Cancelled,
    /// 空闲回收让位给用户；不复用「已取消」，用户没点取消。
    Yielded,
    Failed,
    /// 做完了能做的部分，但有失败项：失败张数、未删旧文件数或未处理张数写在消息里。
    PartialFailed,
}

impl TaskPhase {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled | Self::Yielded | Self::Failed | Self::PartialFailed
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskFailure {
    pub image_id: Option<i64>,
    pub path: String,
    pub reason: String,
}

/// 候选内跳过的一类原因；`key` 与估算视图的图例一致，界面据此取文案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSkipBucket {
    pub key: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub task_id: String,
    pub kind: TaskKind,
    pub label: String,
    pub scope: String,
    pub phase: TaskPhase,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// 候选内跳过按原因细分；只有压缩任务非空，按张数降序。
    pub skip_breakdown: Vec<TaskSkipBucket>,
    /// 后端给出的加权百分比；`None` 表示按「已完成 + 失败 + 已跳过 / 总数」计算。
    /// 压缩固定为 `None`，它的百分比只由计数派生，与环图中心和曲线横轴同源。
    pub progress_percent: Option<u8>,
    pub message: String,
    pub current_image_id: Option<i64>,
    /// 第一次真正进入 `Running` 的时间；排队与准备阶段为空。
    pub run_started_at: Option<i64>,
    /// 当前处理的相对路径；没有时前端回退为 `#图片id`。
    pub current_file: Option<String>,
    /// 已处理字节：压缩任务是已提交批次的原图字节，导出任务是已复制字节；其余为 0。
    pub original_bytes: u64,
    /// 已节省字节；只有压缩任务非 0。
    pub saved_bytes: u64,
    /// 运行以来平均每秒结算的张数（含失败与跳过）；未开始或还没有结算项时为空。
    pub rate_per_second: Option<f64>,
    /// 预计剩余秒数；终态为空。
    pub eta_seconds: Option<u64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub failures: Vec<TaskFailure>,
    pub cancellable: bool,
    /// 这条任务最终写入的操作记录 id；任务进行中通常为空。
    pub operation_id: Option<i64>,
    /// 任务写入的新图片数；目前只有导入任务使用，其他任务为 0。
    pub added_images: usize,
    /// 图库导入时丢弃的临时区同内容行数；其他任务为 0。
    pub removed_temp_images: usize,
    /// 导入目标；只有导入任务有值。
    pub import_target: Option<crate::import::ImportTarget>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSummary {
    pub task_id: String,
    pub kind: TaskKind,
    pub label: String,
    pub scope: String,
    pub phase: TaskPhase,
}

pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "未知 panic".to_owned()
    }
}

/// 把任务体 panic 转成失败终态，返回给调用方的错误文案（与任务消息一致）。
pub(crate) fn fail_from_panic(
    handle: &TaskHandle,
    name: &str,
    payload: &(dyn Any + Send),
) -> String {
    let message = format!("任务异常终止：{}", panic_message(payload));
    log::error!("任务线程 {name} 异常终止：{message}");
    handle.set_message(message.clone());
    handle.finish(TaskPhase::Failed);
    message
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskStartResult {
    Started {
        task: Box<TaskSnapshot>,
    },
    /// 只有 `Fail` 策略会返回：所需资源被占用或已有等待者，调用方据此提示稍后重试。
    Conflict {
        active: Vec<TaskSummary>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    Fail,
    #[default]
    Queue,
}

/// 前端只传语义范围，id 集合在 Rust 侧解析，避免几十万 id 过 IPC。
/// 估算结果要能重建执行请求，所以这里同时支持序列化。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TaskScope {
    Selected {
        #[serde(rename = "imageIds")]
        image_ids: Vec<i64>,
    },
    Folder {
        #[serde(rename = "folderId")]
        folder_id: i64,
        #[serde(rename = "includeSubfolders", default)]
        include_subfolders: bool,
    },
    Collection {
        #[serde(rename = "collectionId")]
        collection_id: i64,
    },
    View {
        query: Box<LibraryQuery>,
    },
    Temp,
    #[default]
    Library,
}

impl TaskScope {
    pub fn label(&self) -> String {
        match self {
            Self::Selected { image_ids } => {
                format!("选中 {} 张", image_ids.len())
            }
            Self::Folder {
                include_subfolders, ..
            } => {
                if *include_subfolders {
                    "当前文件夹及子文件夹".to_owned()
                } else {
                    "当前文件夹".to_owned()
                }
            }
            Self::Collection { .. } => "当前集合".to_owned(),
            Self::View { .. } => "当前视图".to_owned(),
            Self::Temp => "临时文件夹".to_owned(),
            Self::Library => "整个图库".to_owned(),
        }
    }

    pub fn resolve(&self, database: &LibraryDb) -> AppResult<Vec<i64>> {
        match self {
            // 选择集只做去重与校验，不查行是否仍存在；已删除或已被清掉的行交给任务逐张处理。
            Self::Selected { image_ids } => query::normalize_scope_ids(image_ids),
            Self::Folder {
                folder_id,
                include_subfolders,
            } => {
                if *folder_id <= 0 {
                    return Err(AppError::message("文件夹 id 无效"));
                }
                if *include_subfolders {
                    return query::query_ids(
                        database,
                        LibraryQuery {
                            filters: crate::library::query::LibraryFilters {
                                folder_ids: vec![*folder_id],
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    );
                }
                database.with_reader(|connection| {
                    let mut statement = connection.prepare(
                        "SELECT id FROM images
                         WHERE deleted_at IS NULL AND folder_id = ?1
                         ORDER BY id",
                    )?;
                    let ids = statement
                        .query_map([folder_id], |row| row.get::<_, i64>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(ids)
                })
            }
            Self::Collection { collection_id } => {
                if *collection_id <= 0 {
                    return Err(AppError::message("集合 id 无效"));
                }
                query::query_ids(
                    database,
                    LibraryQuery {
                        view: LibraryView::Collection,
                        collection_id: Some(*collection_id),
                        ..Default::default()
                    },
                )
            }
            Self::View { query } => query::query_ids(database, query.as_ref().clone()),
            Self::Temp => query::query_ids(
                database,
                LibraryQuery {
                    view: LibraryView::Temp,
                    ..Default::default()
                },
            ),
            Self::Library => query::query_ids(database, LibraryQuery::default()),
        }
    }

    /// 预览用的范围映射：`Selected` 返回 `None`，由调用方走选择集查询。
    ///
    /// 只读且不改范围语义；任务开始仍先 `resolve` 再按 id 过滤。
    pub fn to_library_query(&self) -> AppResult<Option<LibraryQuery>> {
        Ok(match self {
            Self::Selected { .. } => None,
            Self::Folder {
                folder_id,
                include_subfolders,
            } => {
                if *folder_id <= 0 {
                    return Err(AppError::message("文件夹 id 无效"));
                }
                Some(LibraryQuery {
                    filters: crate::library::query::LibraryFilters {
                        folder_ids: vec![*folder_id],
                        folder_direct: !*include_subfolders,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            }
            Self::Collection { collection_id } => {
                if *collection_id <= 0 {
                    return Err(AppError::message("集合 id 无效"));
                }
                Some(LibraryQuery {
                    view: LibraryView::Collection,
                    collection_id: Some(*collection_id),
                    ..Default::default()
                })
            }
            Self::View { query } => Some(query.as_ref().clone()),
            Self::Temp => Some(LibraryQuery {
                view: LibraryView::Temp,
                ..Default::default()
            }),
            Self::Library => Some(LibraryQuery::default()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum BatchTaskKind {
    #[default]
    Thumbnail,
    AiEmbedding,
    AiPose,
    AiDescription,
    SimilarityGroups,
    OptimizeEstimate,
    Optimize,
    /// 快速检查（只看存在性与大小）。
    CheckFast,
    /// 完整校验（重算 BLAKE3）。
    CheckFull,
    /// 导出到目录。
    Export,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BatchTaskRequest {
    pub kind: BatchTaskKind,
    pub scope: TaskScope,
    pub overwrite: bool,
    pub recalc_visuals: bool,
    pub prompt: Option<String>,
    pub max_tokens: Option<u32>,
    /// 相似分组：两种指标可以单独或同时启用。
    pub group_phash: bool,
    pub phash_threshold: Option<u32>,
    pub group_embedding: bool,
    pub embedding_threshold: Option<f32>,
    /// 空间优化：档位、输出格式与重算开关。
    pub optimize: Option<OptimizeOptions>,
    /// 执行时用于校验估算凭证；估算任务不用。
    pub estimate_id: Option<String>,
    pub estimate_sample_size: Option<usize>,
    /// 导出：目标目录与是否附带 `manifest.csv`。
    pub target_dir: Option<String>,
    pub include_manifest: bool,
    pub conflict: ConflictPolicy,
}

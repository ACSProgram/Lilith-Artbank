//! 统一的后台任务总线。
//!
//! 所有会长时间占用磁盘、写连接或 AI 服务的操作都注册到这里，准入由
//! `concurrency::resources` 的资源表决定：只有真正读写同一资源的任务才互相等待；
//! 长任务冲突时默认按资源 FIFO 排队，路径锁写者优先，短操作仍立即失败。
//! `TaskLane` 只用于任务中心的展示与优先级提示，不再决定冲突。
//! 任务体 panic、线程起不来或提前返回都在这里落到失败终态，并释放资源。
//! 排队中的任务不占任何资源；真正开始时还要核对图库代次，换库后直接取消。
//! 进度通过 `task-progress` / `task-finished` 事件推给前端任务中心。

use std::{
    any::Any,
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::{
    app::error::{AppError, AppResult},
    concurrency::{
        budget::ImageBudget,
        meter::WorkMeter,
        resources::{
            Conflict, HolderInfo, ResourceGuard, ResourceRequest, ResourceTable, WaitError,
        },
    },
    imageops::optimize::OptimizeOptions,
    library::{
        BatchReporter,
        db::LibraryDb,
        query::{self, LibraryQuery, LibraryView},
        service::now_ms,
    },
};

/// 任务历史保留条数（含已结束），只存在内存里；运行中与排队中的任务不会被裁掉。
pub const MAX_TASKS: usize = 50;
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
    pub fn lane(self) -> TaskLane {
        match self {
            Self::Import
            | Self::Thumbnail
            | Self::FolderMove
            | Self::PermanentDelete
            | Self::EmptyTrash
            | Self::Undo
            | Self::Redo
            | Self::ReleaseGarbage
            | Self::Optimize
            | Self::OrphanMove
            | Self::PackCompact => TaskLane::Write,
            Self::AiEmbedding | Self::AiPose | Self::AiDescription | Self::TempFeatures => {
                TaskLane::Ai
            }
            Self::SimilarityGroups | Self::OptimizeEstimate | Self::CheckLibrary | Self::Export => {
                TaskLane::Scan
            }
        }
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskLane {
    Write,
    Ai,
    Scan,
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
    pub lane: TaskLane,
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
    pub lane: TaskLane,
    pub phase: TaskPhase,
}

/// 一个任务的全部可变状态。导入与 AI 任务各自包一层，把旧字段同步到这里。
pub struct TaskHandle {
    id: String,
    kind: TaskKind,
    scope: String,
    cancellable: AtomicBool,
    cancel: AtomicBool,
    phase: Mutex<TaskPhase>,
    total: AtomicUsize,
    completed: AtomicUsize,
    failed: AtomicUsize,
    skipped: AtomicUsize,
    /// 候选内跳过按原因细分；由压缩逐批整体替换，不做累加。
    skip_breakdown: Mutex<Vec<TaskSkipBucket>>,
    /// 加权进度；任务不提供单位时退回 `completed / total`。
    meter: WorkMeter,
    current: Mutex<Option<i64>>,
    current_file: Mutex<Option<String>>,
    original_bytes: AtomicU64,
    saved_bytes: AtomicU64,
    run_started_at: Mutex<Option<i64>>,
    message: Mutex<String>,
    failures: Mutex<Vec<TaskFailure>>,
    started_at: i64,
    finished_at: Mutex<Option<i64>>,
    operation_id: Mutex<Option<i64>>,
    /// 任务写入的新图片数；目前只有导入任务使用。
    added_images: AtomicUsize,
    /// 导入时丢弃的临时区同内容行数；用于同步临时区计数。
    removed_temp_images: AtomicUsize,
    /// 导入目标；只有导入任务有值。
    import_target: Mutex<Option<crate::import::ImportTarget>>,
}

impl TaskHandle {
    pub fn new(id: impl Into<String>, kind: TaskKind, scope: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            kind,
            scope: scope.into(),
            cancellable: AtomicBool::new(true),
            cancel: AtomicBool::new(false),
            phase: Mutex::new(TaskPhase::Queued),
            total: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            skip_breakdown: Mutex::new(Vec::new()),
            meter: WorkMeter::new(),
            current: Mutex::new(None),
            current_file: Mutex::new(None),
            original_bytes: AtomicU64::new(0),
            saved_bytes: AtomicU64::new(0),
            run_started_at: Mutex::new(None),
            message: Mutex::new(String::new()),
            failures: Mutex::new(Vec::new()),
            started_at: now_ms(),
            finished_at: Mutex::new(None),
            operation_id: Mutex::new(None),
            added_images: AtomicUsize::new(0),
            removed_temp_images: AtomicUsize::new(0),
            import_target: Mutex::new(None),
        })
    }

    /// 导入写入新行后更新累计数；前端据此增量更新图库计数。
    pub fn set_added_images(&self, added: usize) {
        self.added_images.store(added, Ordering::Relaxed);
    }

    /// 图库导入丢弃临时区同内容行后累加；这些行要从临时区计数里扣掉。
    pub fn inc_removed_temp_images(&self, count: usize) {
        self.removed_temp_images.fetch_add(count, Ordering::Relaxed);
    }

    pub fn set_import_target(&self, target: crate::import::ImportTarget) {
        *self.import_target.lock() = Some(target);
    }

    pub fn set_operation_id(&self, operation_id: Option<i64>) {
        *self.operation_id.lock() = operation_id;
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn lane(&self) -> TaskLane {
        self.kind.lane()
    }

    pub fn phase(&self) -> TaskPhase {
        *self.phase.lock()
    }

    /// 第一次进入运行阶段的时间；排队与准备阶段为空。
    pub fn run_started_at(&self) -> Option<i64> {
        *self.run_started_at.lock()
    }

    pub fn is_finished(&self) -> bool {
        self.phase().is_terminal()
    }

    pub fn is_running(&self) -> bool {
        !self.is_finished()
    }

    pub fn request_cancel(&self) {
        if self.is_cancellable() {
            self.cancel.store(true, Ordering::SeqCst);
            // 只做显示：任务体还要等当前批次提交完才会落到 `Cancelled`。
            self.set_phase(TaskPhase::Cancelling);
        }
    }

    /// 单条记录的撤销、释放任务不允许中途取消；必须在任务启动前设置。
    pub fn set_cancellable(&self, value: bool) {
        self.cancellable.store(value, Ordering::SeqCst);
    }

    pub fn is_cancellable(&self) -> bool {
        self.cancellable.load(Ordering::SeqCst)
    }

    pub fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn set_phase(&self, phase: TaskPhase) {
        let mut current = self.phase.lock();
        if !current.is_terminal() {
            if phase == TaskPhase::Running {
                self.run_started_at.lock().get_or_insert_with(now_ms);
            }
            *current = phase;
        }
    }

    /// 从“真正开始干活”重新计时；压缩用它排除执行前的逐张复核。
    pub fn mark_work_started(&self) {
        if !self.is_finished() {
            *self.run_started_at.lock() = Some(now_ms());
        }
    }

    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn set_completed(&self, completed: usize) {
        self.completed.store(completed, Ordering::Relaxed);
    }

    /// 按阶段加权后的百分比；只增不减。
    pub fn set_percent(&self, percent: u8) {
        self.meter.set_percent(percent);
    }

    pub fn inc_completed(&self) {
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    /// 已记录的失败条数；失败清单本身最多留 200 条，计数不受这个上限影响。
    pub fn failed(&self) -> usize {
        self.failed.load(Ordering::Relaxed)
    }

    pub fn inc_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// 批量任务结束后按领域层的报告对齐计数，避免 message 与字段各说一套。
    pub fn set_skipped(&self, skipped: usize) {
        self.skipped.store(skipped, Ordering::Relaxed);
    }

    /// 整体替换跳过细分（调用方已经算好累计值，这里不做累加）。
    pub fn set_skip_breakdown(&self, buckets: &[(String, u64)]) {
        *self.skip_breakdown.lock() = buckets
            .iter()
            .map(|(key, count)| TaskSkipBucket {
                key: key.clone(),
                count: *count,
            })
            .collect();
    }

    pub fn set_failed(&self, failed: usize) {
        self.failed.store(failed, Ordering::Relaxed);
    }

    pub fn set_current(&self, image_id: Option<i64>) {
        *self.current.lock() = image_id;
    }

    pub fn set_current_file(&self, path: Option<&str>) {
        *self.current_file.lock() = path.map(str::to_owned);
    }

    pub fn set_bytes(&self, original: u64, saved: u64) {
        self.original_bytes.store(original, Ordering::Relaxed);
        self.saved_bytes.store(saved, Ordering::Relaxed);
    }

    pub fn set_message(&self, message: impl Into<String>) {
        *self.message.lock() = message.into();
    }

    pub fn add_failure(
        &self,
        image_id: Option<i64>,
        path: impl Into<String>,
        reason: impl Into<String>,
    ) {
        let mut failures = self.failures.lock();
        if failures.len() < MAX_FAILURES {
            failures.push(TaskFailure {
                image_id,
                path: path.into(),
                reason: reason.into(),
            });
        }
    }

    pub fn finish(&self, phase: TaskPhase) {
        let phase = if phase.is_terminal() {
            phase
        } else {
            TaskPhase::Completed
        };
        if phase == TaskPhase::Completed {
            self.meter.finish();
        }
        *self.phase.lock() = phase;
        *self.finished_at.lock() = Some(now_ms());
        *self.current.lock() = None;
        *self.current_file.lock() = None;
    }

    /// 按取消标记与执行结果决定终态；这是 runner 的统一收尾。
    pub fn finish_from<T>(&self, result: &AppResult<T>) {
        let phase = if self.should_cancel() {
            TaskPhase::Cancelled
        } else {
            match result {
                Ok(_) => TaskPhase::Completed,
                Err(error) => {
                    self.set_message(error.to_string());
                    TaskPhase::Failed
                }
            }
        };
        self.finish(phase);
    }

    /// 部分完成：有失败张数、未删旧文件或未处理张数时用它，
    /// 让胶囊既不显示「已完成」，也不把做成的部分说成失败。
    pub fn finish_partial(&self, message: impl Into<String>) {
        self.set_message(message);
        self.finish(TaskPhase::PartialFailed);
    }

    pub fn snapshot(&self) -> TaskSnapshot {
        let phase = self.phase();
        let total = self.total.load(Ordering::Relaxed);
        let completed = self.completed.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        let skipped = self.skipped.load(Ordering::Relaxed);
        let finished_at = *self.finished_at.lock();
        let run_started_at = *self.run_started_at.lock();

        // 排队与准备阶段没有开始时间，不拿构造时间凑速度。
        // 速度与「已完成」同一个和：跳过的图同样消耗了完整的编码工作量。
        let rate_per_second = run_started_at.and_then(|start| {
            let elapsed = (finished_at.unwrap_or_else(now_ms) - start) as f64 / 1000.0;
            let worked = completed + failed + skipped;
            (worked > 0 && elapsed >= 0.5).then(|| worked as f64 / elapsed)
        });
        let eta_seconds = if phase.is_terminal() {
            None
        } else {
            run_started_at.and_then(|_| {
                if let Some(eta) = self.meter.eta() {
                    return Some(eta.as_secs().max(1));
                }
                let remaining = total.saturating_sub(completed + failed + skipped);
                let rate = rate_per_second?;
                (remaining > 0 && rate > 0.0)
                    .then(|| ((remaining as f64 / rate).ceil() as u64).max(1))
            })
        };

        TaskSnapshot {
            task_id: self.id.clone(),
            kind: self.kind,
            label: self.kind.label().to_owned(),
            scope: self.scope.clone(),
            lane: self.lane(),
            phase,
            total,
            completed,
            failed,
            skipped,
            skip_breakdown: self.skip_breakdown.lock().clone(),
            // 压缩的百分比只由「已结算张数 / 候选张数」派生（前端与环图、曲线同源），
            // 不再暴露计量器的加权百分比；其他任务沿用各自的阶段加权。
            progress_percent: if self.kind == TaskKind::Optimize {
                None
            } else {
                self.meter.percent()
            },
            message: self.message.lock().clone(),
            current_image_id: *self.current.lock(),
            run_started_at,
            current_file: self.current_file.lock().clone(),
            original_bytes: self.original_bytes.load(Ordering::Relaxed),
            saved_bytes: self.saved_bytes.load(Ordering::Relaxed),
            rate_per_second,
            eta_seconds,
            started_at: self.started_at,
            finished_at,
            failures: self.failures.lock().clone(),
            cancellable: self.is_cancellable(),
            operation_id: *self.operation_id.lock(),
            added_images: self.added_images.load(Ordering::Relaxed),
            removed_temp_images: self.removed_temp_images.load(Ordering::Relaxed),
            import_target: *self.import_target.lock(),
        }
    }

    fn summary(&self) -> TaskSummary {
        TaskSummary {
            task_id: self.id.clone(),
            kind: self.kind,
            label: self.kind.label().to_owned(),
            scope: self.scope.clone(),
            lane: self.lane(),
            phase: self.phase(),
        }
    }
}

/// 任务体退栈时的最后一道兜底：正常返回但没写终态，或 panic 逃过 `catch_unwind` 时标为失败。
struct TaskRunGuard {
    handle: Arc<TaskHandle>,
}

impl Drop for TaskRunGuard {
    fn drop(&mut self) {
        if !self.handle.is_finished() {
            self.handle.set_message("任务异常终止");
            self.handle.finish(TaskPhase::Failed);
        }
    }
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

impl BatchReporter for TaskHandle {
    fn report(&self, completed: usize, total: usize) {
        self.set_total(total);
        self.set_completed(completed);
    }

    fn cancelled(&self) -> bool {
        self.should_cancel()
    }

    fn message(&self, message: &str) {
        self.set_message(message);
    }

    fn percent(&self, percent: u8) {
        self.set_percent(percent);
    }

    fn meter(&self) -> Option<&WorkMeter> {
        Some(&self.meter)
    }

    fn current(&self, image_id: Option<i64>, path: Option<&str>) {
        self.set_current(image_id);
        self.set_current_file(path);
    }

    fn bytes(&self, original: u64, saved: u64) {
        self.set_bytes(original, saved);
    }

    fn skipped_count(&self, skipped: usize) {
        self.set_skipped(skipped);
    }

    fn skip_breakdown(&self, buckets: &[(String, u64)]) {
        self.set_skip_breakdown(buckets);
    }

    fn failed_count(&self, failed: usize) {
        self.set_failed(failed);
    }
}

fn holder_for(handle: &TaskHandle, request: &ResourceRequest) -> HolderInfo {
    let reason = if request.content_writer {
        "正在写入图库内容"
    } else if request.metadata_writer {
        "正在修改图片信息"
    } else if request.ai_service {
        "正在使用 AI 服务"
    } else if request.session_write {
        "正在切换图库"
    } else {
        "正在进行"
    };
    HolderInfo {
        id: handle.id().to_owned(),
        label: handle.kind.label().to_owned(),
        reason: reason.to_owned(),
        path_note: request.path_note,
    }
}

fn conflict_error(conflicts: &[Conflict]) -> AppError {
    let detail = conflicts
        .first()
        .map(Conflict::sentence)
        .unwrap_or_else(|| "有操作正在进行".to_owned());
    AppError::message(format!("{detail}，请等待完成或取消后再试"))
}

#[derive(Default)]
struct BusInner {
    entries: Mutex<VecDeque<Arc<TaskHandle>>>,
}

/// 持有资源准入权；Drop 时由 `ResourceGuard` 释放并唤醒等待者。
pub struct LaneGuard {
    _resource: ResourceGuard,
}

impl LaneGuard {
    fn from_resource(resource: ResourceGuard) -> Self {
        Self {
            _resource: resource,
        }
    }

    pub fn into_resource(self) -> ResourceGuard {
        self._resource
    }
}

impl std::fmt::Debug for LaneGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LaneGuard")
    }
}

#[derive(Clone)]
pub struct TaskBus {
    inner: Arc<BusInner>,
    resources: Arc<ResourceTable>,
    images: Arc<ImageBudget>,
    /// 图库代次：开库、换库、关库时递增。排队中的任务在拿到资源时核对代次，
    /// 发现图库已经换了就直接取消，不会在旧句柄上写库。关库时会前后各递增一次，
    /// 让关库期间提交的任务也一起作废。
    epoch: Arc<AtomicU64>,
}

impl Default for TaskBus {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskBus {
    pub fn new() -> Self {
        Self::with_images(Arc::new(ImageBudget::default()))
    }

    pub fn with_images(images: Arc<ImageBudget>) -> Self {
        Self {
            inner: Arc::new(BusInner::default()),
            resources: Arc::new(ResourceTable::new()),
            images,
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 图库换过一代；由开库/关库调用。
    pub fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn epoch_moved(&self, seen: u64) -> bool {
        self.epoch.load(Ordering::SeqCst) != seen
    }

    pub fn resources(&self) -> Arc<ResourceTable> {
        self.resources.clone()
    }

    pub fn images(&self) -> Arc<ImageBudget> {
        self.images.clone()
    }

    pub fn list(&self) -> Vec<TaskSnapshot> {
        self.inner
            .entries
            .lock()
            .iter()
            .rev()
            .map(|handle| handle.snapshot())
            .collect()
    }

    pub fn active(&self) -> Vec<TaskSummary> {
        self.inner
            .entries
            .lock()
            .iter()
            .filter(|handle| handle.is_running())
            .map(|handle| handle.summary())
            .collect()
    }

    /// 没有运行中任务、排队任务，也没有短操作持有资源。
    pub fn is_idle(&self) -> bool {
        if !self.resources.is_idle() {
            return false;
        }
        self.inner
            .entries
            .lock()
            .iter()
            .all(|handle| handle.is_finished())
    }

    pub fn any_busy(&self) -> bool {
        !self.is_idle()
    }

    fn summaries(&self, conflicts: &[Conflict]) -> Vec<TaskSummary> {
        let entries = self.inner.entries.lock();
        conflicts
            .iter()
            .filter_map(|conflict| {
                entries
                    .iter()
                    .find(|handle| handle.id() == conflict.holder.id)
                    .map(|handle| handle.summary())
            })
            .collect()
    }

    pub fn cancel(&self, task_id: &str) -> AppResult<()> {
        let handle = {
            let entries = self.inner.entries.lock();
            let handle = entries
                .iter()
                .find(|handle| handle.id() == task_id)
                .ok_or_else(|| AppError::message("任务不存在或已结束"))?;
            if handle.is_finished() {
                return Err(AppError::message("任务已经结束"));
            }
            if !handle.is_cancellable() {
                return Err(AppError::message("该任务不能中途取消"));
            }
            handle.clone()
        };
        handle.request_cancel();
        self.resources.remove_waiter(handle.id());
        Ok(())
    }

    /// 关闭/退出时只取消允许中断的任务；内容写任务留给调用方拒绝。
    pub fn cancel_for_close(&self) {
        let cancelled: Vec<Arc<TaskHandle>> = self
            .inner
            .entries
            .lock()
            .iter()
            .filter(|handle| {
                handle.is_running()
                    && handle.phase() != TaskPhase::Queued
                    && handle.kind.can_cancel_for_close()
            })
            .cloned()
            .collect();
        for handle in &cancelled {
            handle.request_cancel();
        }
        let queued: Vec<Arc<TaskHandle>> = self
            .inner
            .entries
            .lock()
            .iter()
            .filter(|handle| {
                handle.phase() == TaskPhase::Queued && handle.kind.can_cancel_for_close()
            })
            .cloned()
            .collect();
        for handle in queued {
            handle.request_cancel();
            self.resources.remove_waiter(handle.id());
        }
    }

    /// 退出/换库前取消所有还在排队的任务，避免关闭过程中队列突然启动。
    pub fn cancel_queued(&self) {
        let cancelled: Vec<Arc<TaskHandle>> = self
            .inner
            .entries
            .lock()
            .iter()
            .filter(|handle| handle.is_running() && handle.phase() == TaskPhase::Queued)
            .cloned()
            .collect();
        for handle in &cancelled {
            handle.request_cancel();
            self.resources.remove_waiter(handle.id());
        }
    }

    /// 等待所有任务结束（含排队任务）；超时返回 false。
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.is_idle() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// 短操作立即申请资源；冲突时返回可读的占用者说明。
    pub fn acquire_immediate(
        &self,
        request: &ResourceRequest,
        holder: &HolderInfo,
    ) -> AppResult<LaneGuard> {
        match self.resources.try_acquire(request, holder) {
            Ok(guard) => Ok(LaneGuard::from_resource(guard)),
            Err(conflicts) => Err(conflict_error(&conflicts)),
        }
    }

    fn register(&self, handle: Arc<TaskHandle>) {
        let mut entries = self.inner.entries.lock();
        entries.push_back(handle);
        while entries.len() > MAX_TASKS {
            let Some(index) = entries.iter().position(|handle| handle.is_finished()) else {
                break;
            };
            entries.remove(index);
        }
    }

    fn spawn_guarded<F>(
        &self,
        name: String,
        handle: Arc<TaskHandle>,
        run: F,
        finish_on_return: bool,
    ) -> AppResult<()>
    where
        F: FnOnce() + Send + 'static,
    {
        let worker = handle.clone();
        let thread_name = name.clone();
        std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                // 排队线程只负责启动任务体，正常返回不代表任务结束，所以不挂这个 guard。
                let _guard = finish_on_return.then(|| TaskRunGuard {
                    handle: worker.clone(),
                });
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                    Ok(()) => {
                        if finish_on_return && !worker.is_finished() {
                            worker.set_message("任务结束但未报告终态");
                            worker.finish(TaskPhase::Failed);
                        }
                    }
                    Err(payload) => {
                        fail_from_panic(&worker, &thread_name, payload.as_ref());
                    }
                }
            })
            .map(|_| ())
            .map_err(|error| {
                log::error!("无法启动任务线程：{error}");
                handle.set_message(format!("无法启动任务线程：{error}"));
                handle.finish(TaskPhase::Failed);
                AppError::message("无法启动任务线程")
            })
    }

    /// 在独立线程里执行任务体。线程起不来或 panic 时把任务标成失败并返回错误，
    /// 否则任务会永远停在“准备中”，还会一直占着 `active()`。
    pub fn spawn_worker<F>(&self, name: String, handle: Arc<TaskHandle>, run: F) -> AppResult<()>
    where
        F: FnOnce() + Send + 'static,
    {
        self.spawn_guarded(name, handle, run, true)
    }

    /// 排队线程只负责等资源后调用启动闭包；闭包返回时任务体可能还在另一个线程里跑，
    /// 所以正常返回不能当成任务结束。
    fn spawn_launcher<F>(&self, name: String, handle: Arc<TaskHandle>, run: F) -> AppResult<()>
    where
        F: FnOnce() + Send + 'static,
    {
        self.spawn_guarded(name, handle, run, false)
    }

    fn run_immediate<F>(&self, handle: Arc<TaskHandle>, name: &str, guard: LaneGuard, run: F)
    where
        F: FnOnce(LaneGuard),
    {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(guard)));
        if let Err(payload) = outcome {
            fail_from_panic(&handle, name, payload.as_ref());
        }
    }

    /// 记为排队；真正入队由各等待线程在资源表内完成，保证按冲突顺序先来先得。
    fn mark_queued(&self, handle: &TaskHandle) {
        handle.set_phase(TaskPhase::Queued);
        handle.set_message("排队等待前一个任务结束");
    }

    /// 给 `run_write_task` 使用：注册任务与进度推送，必要时排队，返回时已经拿到资源。
    /// 调用方应在阻塞线程里调用，不能占着异步运行时工作线程等资源。
    pub fn begin_queued(
        &self,
        app: &AppHandle,
        handle: Arc<TaskHandle>,
        request: ResourceRequest,
    ) -> AppResult<LaneGuard> {
        let stamp = self.epoch();
        self.register(handle.clone());
        spawn_pump(app.clone(), handle.clone());
        let holder = holder_for(&handle, &request);

        if let Ok(guard) = self.resources.try_acquire(&request, &holder) {
            handle.set_phase(TaskPhase::Preparing);
            return Ok(LaneGuard::from_resource(guard));
        }

        self.mark_queued(&handle);
        match self
            .resources
            .acquire(&request, &holder, None, &|| handle.should_cancel())
        {
            Ok(guard) => {
                if let Err(reason) = self.admit_queued(&handle, stamp) {
                    drop(guard);
                    if let Some(reason) = reason {
                        handle.set_message(reason);
                    }
                    handle.finish(TaskPhase::Cancelled);
                    return Err(AppError::message(reason.unwrap_or("任务已取消")));
                }
                handle.set_message("");
                handle.set_phase(TaskPhase::Preparing);
                Ok(LaneGuard::from_resource(guard))
            }
            Err(WaitError::Cancelled) => {
                handle.finish(TaskPhase::Cancelled);
                Err(AppError::message("任务已取消"))
            }
            Err(WaitError::Timeout(_)) => unreachable!("begin_queued 不设超时"),
        }
    }

    /// 启动任务：资源空闲且没有冲突等待者时立即运行；冲突时按 policy 失败或排队。
    pub fn launch<F>(
        &self,
        app: AppHandle,
        handle: Arc<TaskHandle>,
        request: ResourceRequest,
        policy: ConflictPolicy,
        run: F,
    ) -> AppResult<TaskStartResult>
    where
        F: FnOnce(LaneGuard) + Send + 'static,
    {
        let name = format!("task-{}", handle.id());
        let holder = holder_for(&handle, &request);
        let stamp = self.epoch();

        if policy == ConflictPolicy::Fail {
            match self.resources.try_acquire(&request, &holder) {
                Ok(guard) => {
                    self.register(handle.clone());
                    spawn_pump(app, handle.clone());
                    handle.set_phase(TaskPhase::Preparing);
                    self.run_immediate(handle.clone(), &name, LaneGuard::from_resource(guard), run);
                    Ok(TaskStartResult::Started {
                        task: Box::new(handle.snapshot()),
                    })
                }
                Err(conflicts) => Ok(TaskStartResult::Conflict {
                    active: self.summaries(&conflicts),
                }),
            }
        } else {
            self.register(handle.clone());
            spawn_pump(app.clone(), handle.clone());

            if let Ok(guard) = self.resources.try_acquire(&request, &holder) {
                handle.set_phase(TaskPhase::Preparing);
                self.run_immediate(handle.clone(), &name, LaneGuard::from_resource(guard), run);
                return Ok(TaskStartResult::Started {
                    task: Box::new(handle.snapshot()),
                });
            }

            self.mark_queued(&handle);
            let snapshot = handle.snapshot();
            let bus = self.clone();
            let queued = handle.clone();
            let launched = self.spawn_launcher(
                format!("task-queue-{}", queued.id()),
                queued.clone(),
                move || match bus
                    .resources
                    .acquire(&request, &holder, None, &|| queued.should_cancel())
                {
                    Ok(guard) => {
                        if let Err(reason) = bus.admit_queued(&queued, stamp) {
                            drop(guard);
                            if let Some(reason) = reason {
                                queued.set_message(reason);
                            }
                            queued.finish(TaskPhase::Cancelled);
                            return;
                        }
                        queued.set_message("");
                        queued.set_phase(TaskPhase::Preparing);
                        run(LaneGuard::from_resource(guard));
                    }
                    Err(_) => queued.finish(TaskPhase::Cancelled),
                },
            );
            launched?;

            Ok(TaskStartResult::Started {
                task: Box::new(snapshot),
            })
        }
    }

    /// 排队者在拿到资源那一刻的复核：用户已经取消，或排队期间图库被关掉/换掉。
    ///
    /// 返回 `Err(None)` 表示已被取消（消息保持原样），`Err(Some(原因))` 表示代次过期、
    /// 要把原因写给用户。命中时代次说明任务一行都没写，就地取消是安全的。
    fn admit_queued(&self, handle: &TaskHandle, stamp: u64) -> Result<(), Option<&'static str>> {
        if handle.should_cancel() {
            return Err(None);
        }
        if self.epoch_moved(stamp) {
            return Err(Some("图库已切换，任务未开始"));
        }
        Ok(())
    }
}

fn spawn_pump(app: AppHandle, handle: Arc<TaskHandle>) {
    std::thread::spawn(move || {
        loop {
            let snapshot = handle.snapshot();
            let finished = snapshot.phase.is_terminal();
            let _ = app.emit("task-progress", &snapshot);
            if finished {
                let _ = app.emit("task-finished", &snapshot);
                break;
            }
            std::thread::sleep(EMIT_INTERVAL);
        }
    });
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
    /// 立即回收缩略图包，不等空闲条件。
    PackCompact,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_admission_replaces_lane_conflicts() {
        let bus = TaskBus::new();
        let holder = HolderInfo::new("reader", "生成描述");
        let reader = ResourceRequest::session_read()
            .with_ai_service()
            .with_content_writer()
            .with_metadata_writer();
        let _guard = bus.acquire_immediate(&reader, &holder).unwrap();
        // 内容写者与 AI 互斥：编辑/压缩替换内容时不能让 AI 读旧缩略图。
        let error = bus
            .acquire_immediate(
                &ResourceRequest::session_read().with_content_writer(),
                &HolderInfo::new("edit", "保存编辑"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("生成描述"));
        // 文件夹维护只需要路径写，不和 AI/内容写冲突。
        let folder = bus
            .acquire_immediate(
                &ResourceRequest::session_read()
                    .with_timeline(crate::concurrency::resources::Access::Read)
                    .with_paths(vec![crate::concurrency::resources::PathScope::write(
                        "角色",
                    )]),
                &HolderInfo::new("folder", "重命名文件夹"),
            )
            .unwrap();
        drop(folder);
        // 另一个 AI 任务仍然被 ai.service 挡住。
        let error = bus
            .acquire_immediate(
                &ResourceRequest::session_read()
                    .with_ai_service()
                    .with_content_writer(),
                &HolderInfo::new("other", "生成姿势"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("生成描述"));
    }

    #[test]
    fn handle_snapshot_reflects_progress() {
        let handle = TaskHandle::new("t1", TaskKind::Thumbnail, "选中 2 张");
        handle.set_total(2);
        handle.inc_completed();
        handle.inc_failed();
        handle.set_current(Some(7));
        handle.add_failure(Some(8), "", "无法解码");
        handle.set_phase(TaskPhase::Running);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.total, 2);
        assert_eq!(snapshot.completed, 1);
        assert_eq!(snapshot.failed, 1);
        assert_eq!(snapshot.current_image_id, Some(7));
        assert_eq!(snapshot.failures.len(), 1);
        assert!(!snapshot.phase.is_terminal());

        handle.finish(TaskPhase::Completed);
        let snapshot = handle.snapshot();
        assert!(snapshot.phase.is_terminal());
        assert_eq!(snapshot.current_image_id, None);
    }

    #[test]
    fn scopes_serialize_with_camel_case_fields() {
        let scope: TaskScope =
            serde_json::from_str(r#"{"type": "folder", "folderId": 4, "includeSubfolders": true}"#)
                .unwrap();
        assert!(matches!(
            scope,
            TaskScope::Folder {
                folder_id: 4,
                include_subfolders: true
            }
        ));
        let scope: TaskScope =
            serde_json::from_str(r#"{"type": "selected", "imageIds": [3, 1]}"#).unwrap();
        assert_eq!(scope.label(), "选中 2 张");
        let scope: TaskScope = serde_json::from_str(r#"{"type": "library"}"#).unwrap();
        assert_eq!(scope.label(), "整个图库");
    }

    #[test]
    fn task_start_result_is_tagged() {
        let value = serde_json::to_value(TaskStartResult::Conflict {
            active: vec![TaskSummary {
                task_id: "a".into(),
                kind: TaskKind::Import,
                label: "导入".into(),
                scope: "整个图库".into(),
                lane: TaskLane::Write,
                phase: TaskPhase::Running,
            }],
        })
        .unwrap();
        assert_eq!(value["status"], "conflict");
        assert_eq!(value["active"][0]["taskId"], "a");
        assert_eq!(value["active"][0]["lane"], "write");
    }

    #[test]
    fn non_cancellable_tasks_ignore_cancel_requests() {
        let bus = TaskBus::new();
        let handle = TaskHandle::new("u1", TaskKind::Undo, "撤销 1 条记录");
        handle.set_cancellable(false);
        handle.set_phase(TaskPhase::Running);
        bus.register(handle.clone());
        assert!(bus.cancel("u1").is_err());
        assert!(!handle.should_cancel());
        assert!(!handle.snapshot().cancellable);

        handle.set_cancellable(true);
        bus.cancel("u1").unwrap();
        assert!(handle.should_cancel());
        assert!(handle.snapshot().cancellable);
    }

    #[test]
    fn cancel_request_marks_phase_cancelling_until_terminal() {
        let handle = TaskHandle::new("c1", TaskKind::Thumbnail, "选中 3 张");
        handle.set_phase(TaskPhase::Running);
        handle.request_cancel();
        assert!(handle.should_cancel());
        assert_eq!(handle.phase(), TaskPhase::Cancelling);
        // 取消中不是终态：任务仍在跑，前端要继续轮询进度。
        assert!(!TaskPhase::Cancelling.is_terminal());
        assert!(handle.is_running());

        handle.finish(TaskPhase::Cancelled);
        handle.request_cancel();
        assert_eq!(
            handle.phase(),
            TaskPhase::Cancelled,
            "终态之后相位不应再被改写"
        );
    }

    #[test]
    fn undo_and_release_run_on_write_lane() {
        assert_eq!(TaskKind::Undo.lane(), TaskLane::Write);
        assert_eq!(TaskKind::Redo.lane(), TaskLane::Write);
        assert_eq!(TaskKind::ReleaseGarbage.lane(), TaskLane::Write);
        assert_eq!(TaskKind::Undo.label(), "撤销操作");
        assert_eq!(TaskKind::ReleaseGarbage.label(), "释放删除文件");
    }

    fn wait_terminal(handle: &TaskHandle) {
        for _ in 0..200 {
            if handle.is_finished() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("任务没有在预期时间内结束");
    }

    #[test]
    fn panicking_worker_finishes_failed_and_releases_resources() {
        let bus = TaskBus::new();
        let guard = bus
            .acquire_immediate(
                &ResourceRequest::session_read(),
                &HolderInfo::new("panic-holder", "测试"),
            )
            .unwrap();
        let handle = TaskHandle::new("panic", TaskKind::Thumbnail, "选中 1 张");
        let worker = handle.clone();
        bus.spawn_worker("panic-test".to_owned(), handle.clone(), move || {
            let _guard = guard;
            panic!("boom");
        })
        .unwrap();
        wait_terminal(&handle);
        assert_eq!(worker.phase(), TaskPhase::Failed);
        assert!(worker.snapshot().message.contains("boom"));
        assert!(bus.is_idle());
    }

    #[test]
    fn worker_returning_without_terminal_is_marked_failed() {
        let bus = TaskBus::new();
        let handle = TaskHandle::new("early", TaskKind::Thumbnail, "选中 1 张");
        bus.spawn_worker("early-return".to_owned(), handle.clone(), || {})
            .unwrap();
        wait_terminal(&handle);
        assert_eq!(handle.phase(), TaskPhase::Failed);
        assert!(handle.snapshot().message.contains("未报告终态"));
    }

    #[test]
    fn snapshot_reports_run_time_rate_and_eta() {
        let handle = TaskHandle::new("timed", TaskKind::Thumbnail, "选中 10 张");
        assert!(handle.snapshot().run_started_at.is_none());
        assert!(handle.snapshot().rate_per_second.is_none());
        assert!(handle.snapshot().eta_seconds.is_none());

        handle.set_total(10);
        handle.set_phase(TaskPhase::Queued);
        assert!(
            handle.snapshot().run_started_at.is_none(),
            "排队时间不应算进运行时长"
        );
        handle.set_phase(TaskPhase::Running);
        assert!(handle.snapshot().run_started_at.is_some());

        handle.set_completed(2);
        handle.set_current_file(Some("人物/a.jpg"));
        std::thread::sleep(Duration::from_millis(600));
        let snapshot = handle.snapshot();
        assert!(snapshot.rate_per_second.is_some());
        assert!(snapshot.eta_seconds.is_some());
        assert_eq!(snapshot.current_file.as_deref(), Some("人物/a.jpg"));

        handle.finish(TaskPhase::Completed);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.current_file, None);
        assert_eq!(snapshot.eta_seconds, None);
    }

    /// 全部跳过时速度同样有值：跳过的图也消耗了完整的工作量。
    #[test]
    fn rate_counts_skipped_work() {
        let handle = TaskHandle::new("skips", TaskKind::Optimize, "选中 10 张");
        handle.set_total(10);
        handle.set_phase(TaskPhase::Running);
        handle.set_skipped(4);
        std::thread::sleep(Duration::from_millis(600));
        let snapshot = handle.snapshot();
        assert!(snapshot.completed == 0 && snapshot.failed == 0);
        let rate = snapshot.rate_per_second.expect("全跳过时速度不能为空");
        assert!(rate > 0.0, "{rate}");
        // ETA 与速度同源：还剩 6 张没结算。
        assert!(snapshot.eta_seconds.is_some());
    }

    /// 压缩不暴露计量器加权百分比，四个位置读同一个「已结算 / 候选张数」。
    #[test]
    fn optimize_snapshot_percent_comes_from_counters() {
        let handle = TaskHandle::new("opt", TaskKind::Optimize, "整个图库");
        handle.set_total(100);
        handle.set_completed(40);
        handle.set_skipped(22);
        handle.set_failed(2);
        handle.set_percent(73);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.progress_percent, None, "压缩不该带加权百分比");

        // 其他任务仍用计量器的阶段加权。
        let other = TaskHandle::new("imp", TaskKind::Import, "选中 100 张");
        other.set_total(100);
        other.set_percent(73);
        assert_eq!(other.snapshot().progress_percent, Some(73));
    }

    /// 跳过细分整体替换，前一批的桶不会残留。
    #[test]
    fn skip_breakdown_replaces_previous_buckets() {
        let handle = TaskHandle::new("buckets", TaskKind::Optimize, "整个图库");
        handle.set_skip_breakdown(&[("quality".to_owned(), 3), ("depth".to_owned(), 1)]);
        assert_eq!(handle.snapshot().skip_breakdown.len(), 2);
        handle.set_skip_breakdown(&[("notSmaller".to_owned(), 7)]);
        let breakdown = handle.snapshot().skip_breakdown;
        assert_eq!(breakdown.len(), 1);
        assert_eq!(
            (breakdown[0].key.as_str(), breakdown[0].count),
            ("notSmaller", 7)
        );
    }

    /// 让位是终态：用户没点取消，胶囊上是「已让位」而不是「已取消」。
    #[test]
    fn yielded_is_terminal_and_not_retried() {
        let handle = TaskHandle::new("y1", TaskKind::PackCompact, "空闲回收");
        handle.set_phase(TaskPhase::Running);
        handle.set_message("已让位 · 已回收 1 个段");
        handle.finish(TaskPhase::Yielded);
        let snapshot = handle.snapshot();
        assert!(snapshot.phase.is_terminal());
        assert_eq!(snapshot.phase, TaskPhase::Yielded);
        assert!(handle.is_finished());
        assert_eq!(TaskKind::PackCompact.lane(), TaskLane::Write);
        assert_eq!(TaskKind::PackCompact.label(), "回收缩略图包");

        // 终态之后取消请求不再改写相位。
        handle.request_cancel();
        assert_eq!(handle.phase(), TaskPhase::Yielded);
        // 让位的任务不参与「还在跑」的判断，排队中的下一个可以启动。
        let bus = TaskBus::new();
        bus.register(handle.clone());
        assert!(bus.is_idle());
    }

    #[test]
    fn maintenance_task_kinds_have_lanes_and_labels() {
        assert_eq!(TaskKind::CheckLibrary.lane(), TaskLane::Scan);
        assert_eq!(TaskKind::Export.lane(), TaskLane::Scan);
        assert_eq!(TaskKind::OrphanMove.lane(), TaskLane::Write);
        assert_eq!(TaskKind::CheckLibrary.label(), "检查图库");
        assert_eq!(TaskKind::Export.label(), "导出图片");
        assert_eq!(TaskKind::OrphanMove.label(), "清理孤儿文件");
        // 关库时不阻塞的：检查、导出、回收与释放；孤儿移动要等它做完。
        assert!(TaskKind::CheckLibrary.can_cancel_for_close());
        assert!(TaskKind::Export.can_cancel_for_close());
        assert!(TaskKind::PackCompact.can_cancel_for_close());
        assert!(TaskKind::ReleaseGarbage.can_cancel_for_close());
        assert!(!TaskKind::OrphanMove.can_cancel_for_close());
    }

    #[test]
    fn history_trim_keeps_running_tasks() {
        let bus = TaskBus::new();
        let running = TaskHandle::new("running", TaskKind::Import, "运行中");
        running.set_phase(TaskPhase::Running);
        bus.register(running.clone());
        for index in 0..60 {
            let handle = TaskHandle::new(format!("done-{index}"), TaskKind::Import, "已完成");
            handle.finish(TaskPhase::Completed);
            bus.register(handle);
        }
        let tasks = bus.list();
        assert!(tasks.iter().any(|task| task.task_id == "running"));
        assert_eq!(tasks.len(), MAX_TASKS);
    }

    /// 排队期间换库（关库/开库）：拿到资源时按代次取消，不再碰旧库。
    #[test]
    fn a_queued_task_is_dropped_when_the_library_epoch_moves() {
        let bus = TaskBus::new();
        let handle = TaskHandle::new("queued", TaskKind::Thumbnail, "选中 3 张");
        let stamp = bus.epoch();
        assert!(bus.admit_queued(&handle, stamp).is_ok());

        bus.bump_epoch();
        let refused = bus.admit_queued(&handle, stamp);
        assert_eq!(refused, Err(Some("图库已切换，任务未开始")));

        // 新提交的任务拿到的是新代次，不受影响。
        let fresh = bus.epoch();
        assert!(bus.admit_queued(&handle, fresh).is_ok());
    }

    /// 取消优先于代次：用户点过取消的任务不该被写成「图库已切换」。
    #[test]
    fn a_cancelled_queued_task_keeps_its_own_message() {
        let bus = TaskBus::new();
        let handle = TaskHandle::new("queued", TaskKind::Thumbnail, "选中 3 张");
        let stamp = bus.epoch();
        bus.bump_epoch();
        handle.request_cancel();
        assert_eq!(bus.admit_queued(&handle, stamp), Err(None));
    }

    /// 部分失败是终态并保留说明；取消按钮不再出现。
    #[test]
    fn partial_failure_is_terminal_and_keeps_the_message() {
        let handle = TaskHandle::new("partial", TaskKind::PackCompact, "立即回收");
        handle.set_phase(TaskPhase::Running);
        handle.finish_partial("已回收 1 个段 · 释放 12 MB · 2 个旧段删除失败");
        let snapshot = handle.snapshot();
        assert!(snapshot.phase.is_terminal());
        assert_eq!(snapshot.phase, TaskPhase::PartialFailed);
        assert!(snapshot.message.contains("删除失败"));
        assert!(handle.is_finished());
    }
}

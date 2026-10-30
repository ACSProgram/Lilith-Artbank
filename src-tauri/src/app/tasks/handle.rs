//! 任务句柄、资源准入凭据与占用者文案。

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use parking_lot::Mutex;

use crate::{
    app::error::{AppError, AppResult},
    concurrency::{
        meter::WorkMeter,
        resources::{Conflict, HolderInfo, ResourceGuard, ResourceRequest},
    },
    library::{BatchReporter, service::now_ms},
};

use super::{
    MAX_FAILURES, TaskFailure, TaskKind, TaskPhase, TaskSkipBucket, TaskSnapshot, TaskSummary,
};

/// 一个任务的全部可变状态。导入与 AI 任务各自包一层，把旧字段同步到这里。
pub struct TaskHandle {
    id: String,
    pub(super) kind: TaskKind,
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

    pub(super) fn summary(&self) -> TaskSummary {
        TaskSummary {
            task_id: self.id.clone(),
            kind: self.kind,
            label: self.kind.label().to_owned(),
            scope: self.scope.clone(),
            phase: self.phase(),
        }
    }
}

/// 任务体退栈时的最后一道兜底：正常返回但没写终态，或 panic 逃过 `catch_unwind` 时标为失败。
pub(super) struct TaskRunGuard {
    pub(super) handle: Arc<TaskHandle>,
}

impl Drop for TaskRunGuard {
    fn drop(&mut self) {
        if !self.handle.is_finished() {
            self.handle.set_message("任务异常终止");
            self.handle.finish(TaskPhase::Failed);
        }
    }
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

pub(super) fn holder_for(handle: &TaskHandle, request: &ResourceRequest) -> HolderInfo {
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

pub(super) fn conflict_error(conflicts: &[Conflict]) -> AppError {
    let detail = conflicts
        .first()
        .map(Conflict::sentence)
        .unwrap_or_else(|| "有操作正在进行".to_owned());
    AppError::message(format!("{detail}，请等待完成或取消后再试"))
}

/// 持有资源准入权；Drop 时由 `ResourceGuard` 释放并唤醒等待者。
pub struct LaneGuard {
    _resource: ResourceGuard,
}

impl LaneGuard {
    pub(super) fn from_resource(resource: ResourceGuard) -> Self {
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

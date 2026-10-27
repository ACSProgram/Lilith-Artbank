//! 统一的后台任务总线。
//!
//! 所有会长时间占用磁盘、写连接或 AI 服务的操作都注册到这里：
//! 同一时刻只允许一个 `write` 任务；`ai` 与 `scan` 可以并行；同 lane 串行。
//! 长任务冲突时默认排队（同 lane FIFO，write 优先），短操作仍立即失败。
//! 任务体 panic、线程起不来或提前返回都在这里落到失败终态，并释放 lane。
//! 进度通过 `task-progress` / `task-finished` 事件推给前端任务中心。

use std::{
    any::Any,
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::{
    app::error::{AppError, AppResult},
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
const MAX_SCOPE_IDS: usize = 1_000_000;

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
    /// 仅用于临时区的 embedding/pose 生成；占用 write lane。
    TempFeatures,
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
            | Self::TempFeatures => TaskLane::Write,
            Self::AiEmbedding | Self::AiPose | Self::AiDescription => TaskLane::Ai,
            Self::SimilarityGroups | Self::OptimizeEstimate => TaskLane::Scan,
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskLane {
    Write,
    Ai,
    Scan,
}

impl TaskLane {
    /// 两个 lane 是否会互相阻塞。write 与任何 lane 冲突，ai 与 scan 可以并行。
    fn conflicts_with(self, other: TaskLane) -> bool {
        self == Self::Write || other == Self::Write || self == other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskPhase {
    Queued,
    Preparing,
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl TaskPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskFailure {
    pub image_id: Option<i64>,
    pub path: String,
    pub reason: String,
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
    /// 后端给出的加权百分比；`None` 表示按 `completed / total` 计算。
    pub progress_percent: Option<u8>,
    pub message: String,
    pub current_image_id: Option<i64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub failures: Vec<TaskFailure>,
    pub cancellable: bool,
    /// 这条任务最终写入的操作记录 id；任务进行中通常为空。
    pub operation_id: Option<i64>,
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
    /// 后端按耗时加权后的百分比；`None` 时前端按 completed / total 计算。
    percent: Mutex<Option<u8>>,
    current: Mutex<Option<i64>>,
    message: Mutex<String>,
    failures: Mutex<Vec<TaskFailure>>,
    started_at: i64,
    finished_at: Mutex<Option<i64>>,
    operation_id: Mutex<Option<i64>>,
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
            percent: Mutex::new(None),
            current: Mutex::new(None),
            message: Mutex::new(String::new()),
            failures: Mutex::new(Vec::new()),
            started_at: now_ms(),
            finished_at: Mutex::new(None),
            operation_id: Mutex::new(None),
        })
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

    pub fn is_finished(&self) -> bool {
        self.phase().is_terminal()
    }

    pub fn is_running(&self) -> bool {
        !self.is_finished()
    }

    pub fn request_cancel(&self) {
        if self.is_cancellable() {
            self.cancel.store(true, Ordering::SeqCst);
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
            *current = phase;
        }
    }

    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn set_completed(&self, completed: usize) {
        self.completed.store(completed, Ordering::Relaxed);
    }

    /// 按阶段加权后的百分比；只在阶段单位不可比时使用。
    pub fn set_percent(&self, percent: u8) {
        *self.percent.lock() = Some(percent.min(100));
    }

    pub fn inc_completed(&self) {
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// 批量任务结束后按领域层的报告对齐计数，避免 message 与字段各说一套。
    pub fn set_skipped(&self, skipped: usize) {
        self.skipped.store(skipped, Ordering::Relaxed);
    }

    pub fn set_failed(&self, failed: usize) {
        self.failed.store(failed, Ordering::Relaxed);
    }

    pub fn set_current(&self, image_id: Option<i64>) {
        *self.current.lock() = image_id;
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
        *self.phase.lock() = phase;
        *self.finished_at.lock() = Some(now_ms());
        *self.current.lock() = None;
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

    pub fn snapshot(&self) -> TaskSnapshot {
        TaskSnapshot {
            task_id: self.id.clone(),
            kind: self.kind,
            label: self.kind.label().to_owned(),
            scope: self.scope.clone(),
            lane: self.lane(),
            phase: self.phase(),
            total: self.total.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            progress_percent: *self.percent.lock(),
            message: self.message.lock().clone(),
            current_image_id: *self.current.lock(),
            started_at: self.started_at,
            finished_at: *self.finished_at.lock(),
            failures: self.failures.lock().clone(),
            cancellable: self.is_cancellable(),
            operation_id: *self.operation_id.lock(),
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
}

/// 排队中的任务：只在等待 lane 时存在，拿到 lane 或取消后即移除。
#[derive(Debug)]
struct WaitEntry {
    task_id: String,
    lane: TaskLane,
}

#[derive(Default)]
struct LaneState {
    write: usize,
    ai: usize,
    scan: usize,
    waiters: VecDeque<WaitEntry>,
}

impl LaneState {
    fn can_start(&self, lane: TaskLane) -> bool {
        let occupied = self.write + self.ai + self.scan;
        match lane {
            TaskLane::Write => occupied == 0,
            // ai 与 scan 可以并行，但仍然不能与 write 并行。
            TaskLane::Ai => self.write == 0 && self.ai == 0,
            TaskLane::Scan => self.write == 0 && self.scan == 0,
        }
    }

    fn enter(&mut self, lane: TaskLane) {
        match lane {
            TaskLane::Write => self.write += 1,
            TaskLane::Ai => self.ai += 1,
            TaskLane::Scan => self.scan += 1,
        }
    }

    fn leave(&mut self, lane: TaskLane) {
        match lane {
            TaskLane::Write => self.write = self.write.saturating_sub(1),
            TaskLane::Ai => self.ai = self.ai.saturating_sub(1),
            TaskLane::Scan => self.scan = self.scan.saturating_sub(1),
        }
    }

    fn idle_unchecked(&self) -> bool {
        self.write == 0 && self.ai == 0 && self.scan == 0 && self.waiters.is_empty()
    }

    fn enqueue(&mut self, handle: &TaskHandle) {
        if self
            .waiters
            .iter()
            .any(|waiter| waiter.task_id == handle.id())
        {
            return;
        }
        self.waiters.push_back(WaitEntry {
            task_id: handle.id().to_owned(),
            lane: handle.lane(),
        });
    }

    fn remove_waiter(&mut self, task_id: &str) {
        self.waiters.retain(|waiter| waiter.task_id != task_id);
    }

    /// 等待者能否开始：同 lane 先来先得；write 只在全空闲且排在最前时开始；
    /// 任何 write 等待者都会阻止它后面的 ai/scan 启动，避免写任务被持续插队饿死。
    fn waiter_can_start(&self, task_id: &str, lane: TaskLane) -> bool {
        let Some(position) = self
            .waiters
            .iter()
            .position(|waiter| waiter.task_id == task_id)
        else {
            return false;
        };
        let ahead: Vec<&WaitEntry> = self.waiters.iter().take(position).collect();
        let write_ahead = ahead.iter().any(|waiter| waiter.lane == TaskLane::Write);
        match lane {
            TaskLane::Write => self.can_start(TaskLane::Write) && ahead.is_empty(),
            TaskLane::Ai => {
                self.can_start(TaskLane::Ai)
                    && !write_ahead
                    && !ahead.iter().any(|waiter| waiter.lane == TaskLane::Ai)
            }
            TaskLane::Scan => {
                self.can_start(TaskLane::Scan)
                    && !write_ahead
                    && !ahead.iter().any(|waiter| waiter.lane == TaskLane::Scan)
            }
        }
    }

    fn has_waiters(&self) -> bool {
        !self.waiters.is_empty()
    }
}

#[derive(Default)]
struct BusInner {
    lanes: Mutex<LaneState>,
    ready: Condvar,
    entries: Mutex<VecDeque<Arc<TaskHandle>>>,
}

/// 持有某个 lane 的占用权；Drop 时释放并唤醒排队任务。
pub struct LaneGuard {
    inner: Arc<BusInner>,
    lane: TaskLane,
}

impl Drop for LaneGuard {
    fn drop(&mut self) {
        self.inner.lanes.lock().leave(self.lane);
        self.inner.ready.notify_all();
    }
}

#[derive(Clone, Default)]
pub struct TaskBus {
    inner: Arc<BusInner>,
}

impl TaskBus {
    pub fn new() -> Self {
        Self::default()
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

    /// lane 是否被占用。短操作（编辑、文件夹元数据）没有任务条目，
    /// 只能通过 lane 计数判断。
    pub fn lane_busy(&self, lane: TaskLane) -> bool {
        let state = self.inner.lanes.lock();
        match lane {
            TaskLane::Write => state.write > 0,
            TaskLane::Ai => state.ai > 0,
            TaskLane::Scan => state.scan > 0,
        }
    }

    /// 没有运行中任务、排队任务，也没有短操作占着 lane。
    pub fn is_idle(&self) -> bool {
        if !self.inner.lanes.lock().idle_unchecked() {
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

    pub fn conflicting(&self, lane: TaskLane) -> Vec<TaskSummary> {
        self.inner
            .entries
            .lock()
            .iter()
            .filter(|handle| handle.is_running() && lane.conflicts_with(handle.lane()))
            .map(|handle| handle.summary())
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
        // 排队中的任务也要立刻醒来，看到取消标记后结束。
        self.inner.lanes.lock().remove_waiter(handle.id());
        self.inner.ready.notify_all();
        Ok(())
    }

    pub fn cancel_lanes(&self, lanes: &[TaskLane]) {
        {
            let entries = self.inner.entries.lock();
            for handle in entries.iter() {
                if handle.is_running() && lanes.contains(&handle.lane()) {
                    handle.request_cancel();
                }
            }
        }
        {
            let mut state = self.inner.lanes.lock();
            state.waiters.retain(|waiter| !lanes.contains(&waiter.lane));
        }
        self.inner.ready.notify_all();
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
        }
        self.inner.lanes.lock().waiters.clear();
        self.inner.ready.notify_all();
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

    /// 短操作（编辑、文件夹元数据）立即占用 write lane；有任务在跑时直接报错。
    pub fn acquire_immediate(&self, lane: TaskLane) -> AppResult<LaneGuard> {
        let mut state = self.inner.lanes.lock();
        if !state.can_start(lane) {
            drop(state);
            let active = self.conflicting(lane);
            let label = active
                .first()
                .map(|task| task.label.as_str())
                .unwrap_or("其他任务");
            return Err(AppError::message(format!(
                "「{label}」正在进行，请等待完成或取消后再试"
            )));
        }
        state.enter(lane);
        Ok(LaneGuard {
            inner: self.inner.clone(),
            lane,
        })
    }

    /// 阻塞等待 lane：先幂等入队（调用方可能已经入队保序），每 200ms 醒一次检查取消标记；
    /// 被取消时返回 `None`，不留下等待项，也不占用 lane。
    fn acquire_blocking(&self, lane: TaskLane, handle: &TaskHandle) -> Option<LaneGuard> {
        let mut state = self.inner.lanes.lock();
        state.enqueue(handle);
        loop {
            if handle.should_cancel() {
                state.remove_waiter(handle.id());
                return None;
            }
            if state.waiter_can_start(handle.id(), lane) {
                state.remove_waiter(handle.id());
                state.enter(lane);
                return Some(LaneGuard {
                    inner: self.inner.clone(),
                    lane,
                });
            }
            self.inner
                .ready
                .wait_for(&mut state, Duration::from_millis(200));
        }
    }

    fn can_start_now(&self, lane: TaskLane) -> bool {
        let state = self.inner.lanes.lock();
        !state.has_waiters() && state.can_start(lane)
    }

    fn has_waiters(&self) -> bool {
        self.inner.lanes.lock().has_waiters()
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

    /// 排队线程只负责等 lane 后调用启动闭包；闭包返回时任务体可能还在另一个线程里跑，
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

    /// 记为排队并进入等待队列。入队必须由调用线程同步完成：交给排队线程自己入队的话，
    /// 后提交的任务可能先进队列，FIFO 就失效了。
    fn mark_queued(&self, handle: &TaskHandle) {
        handle.set_phase(TaskPhase::Queued);
        handle.set_message("排队等待前一个任务结束");
        self.inner.lanes.lock().enqueue(handle);
    }

    /// 给 `run_write_task` 使用：注册任务与进度推送，必要时排队，返回时已经拿到 lane。
    /// 调用方应在阻塞线程里调用，不能占着异步运行时工作线程等 lane。
    pub fn begin_queued(&self, app: &AppHandle, handle: Arc<TaskHandle>) -> AppResult<LaneGuard> {
        let lane = handle.lane();
        self.register(handle.clone());
        spawn_pump(app.clone(), handle.clone());

        if self.can_start_now(lane)
            && let Ok(guard) = self.acquire_immediate(lane)
        {
            handle.set_phase(TaskPhase::Preparing);
            return Ok(guard);
        }

        self.mark_queued(&handle);
        let Some(guard) = self.acquire_blocking(lane, &handle) else {
            self.inner.lanes.lock().remove_waiter(handle.id());
            handle.finish(TaskPhase::Cancelled);
            return Err(AppError::message("任务已取消"));
        };
        if handle.should_cancel() {
            drop(guard);
            handle.finish(TaskPhase::Cancelled);
            return Err(AppError::message("任务已取消"));
        }
        handle.set_message("");
        handle.set_phase(TaskPhase::Preparing);
        Ok(guard)
    }

    /// 启动任务：lane 空闲且没有等待者时立即运行；冲突时按 policy 失败或排队。
    pub fn launch<F>(
        &self,
        app: AppHandle,
        handle: Arc<TaskHandle>,
        policy: ConflictPolicy,
        run: F,
    ) -> AppResult<TaskStartResult>
    where
        F: FnOnce(LaneGuard) + Send + 'static,
    {
        let lane = handle.lane();
        let name = format!("task-{}", handle.id());

        if policy == ConflictPolicy::Fail {
            let active = self.conflicting(lane);
            if !active.is_empty() || self.lane_busy(lane) || self.has_waiters() {
                return Ok(TaskStartResult::Conflict { active });
            }
            let Ok(guard) = self.acquire_immediate(lane) else {
                return Ok(TaskStartResult::Conflict {
                    active: self.conflicting(lane),
                });
            };
            self.register(handle.clone());
            spawn_pump(app, handle.clone());
            handle.set_phase(TaskPhase::Preparing);
            self.run_immediate(handle.clone(), &name, guard, run);
            return Ok(TaskStartResult::Started {
                task: handle.snapshot(),
            });
        }

        self.register(handle.clone());
        spawn_pump(app.clone(), handle.clone());

        if self.can_start_now(lane)
            && let Ok(guard) = self.acquire_immediate(lane)
        {
            handle.set_phase(TaskPhase::Preparing);
            self.run_immediate(handle.clone(), &name, guard, run);
            return Ok(TaskStartResult::Started {
                task: handle.snapshot(),
            });
        }

        self.mark_queued(&handle);
        let snapshot = handle.snapshot();
        let bus = self.clone();
        let queued = handle.clone();
        let launched = self.spawn_launcher(
            format!("task-queue-{}", queued.id()),
            queued.clone(),
            move || {
                let Some(guard) = bus.acquire_blocking(queued.lane(), &queued) else {
                    bus.inner.lanes.lock().remove_waiter(queued.id());
                    queued.finish(TaskPhase::Cancelled);
                    return;
                };
                if queued.should_cancel() {
                    drop(guard);
                    bus.inner.lanes.lock().remove_waiter(queued.id());
                    queued.finish(TaskPhase::Cancelled);
                    return;
                }
                queued.set_message("");
                queued.set_phase(TaskPhase::Preparing);
                run(guard);
            },
        );
        if let Err(error) = launched {
            self.inner.lanes.lock().remove_waiter(handle.id());
            return Err(error);
        }

        Ok(TaskStartResult::Started { task: snapshot })
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
        task: TaskSnapshot,
    },
    /// 只有 `Fail` 策略会返回：lane 被占用或已有等待者，调用方据此提示稍后重试。
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
            Self::Selected { image_ids } => {
                let mut ids = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for id in image_ids {
                    if *id <= 0 {
                        return Err(AppError::message("图片 id 无效"));
                    }
                    if seen.insert(*id) {
                        ids.push(*id);
                    }
                }
                if ids.len() > MAX_SCOPE_IDS {
                    return Err(AppError::message("单次任务最多处理 100 万张图片"));
                }
                Ok(ids)
            }
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
    pub conflict: ConflictPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_rules_match_plan() {
        let mut state = LaneState::default();
        assert!(state.can_start(TaskLane::Write));
        state.enter(TaskLane::Ai);
        assert!(!state.can_start(TaskLane::Write));
        assert!(!state.can_start(TaskLane::Ai));
        assert!(state.can_start(TaskLane::Scan));
        state.enter(TaskLane::Scan);
        assert!(!state.can_start(TaskLane::Write));
        assert!(!state.can_start(TaskLane::Scan));
        state.leave(TaskLane::Ai);
        state.leave(TaskLane::Scan);
        assert!(state.idle_unchecked());
        assert!(state.can_start(TaskLane::Write));
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
    fn panicking_worker_finishes_failed_and_releases_lane() {
        let bus = TaskBus::new();
        let guard = bus.acquire_immediate(TaskLane::Write).unwrap();
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
        assert!(!bus.lane_busy(TaskLane::Write));
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

    #[test]
    fn queued_tasks_are_fifo_and_write_has_priority() {
        let mut state = LaneState::default();
        state.waiters.push_back(WaitEntry {
            task_id: "ai-1".into(),
            lane: TaskLane::Ai,
        });
        state.waiters.push_back(WaitEntry {
            task_id: "write-1".into(),
            lane: TaskLane::Write,
        });
        state.waiters.push_back(WaitEntry {
            task_id: "scan-1".into(),
            lane: TaskLane::Scan,
        });
        assert!(state.waiter_can_start("ai-1", TaskLane::Ai));
        assert!(!state.waiter_can_start("write-1", TaskLane::Write));
        assert!(!state.waiter_can_start("scan-1", TaskLane::Scan));

        state.remove_waiter("ai-1");
        assert!(state.waiter_can_start("write-1", TaskLane::Write));

        state.enter(TaskLane::Ai);
        assert!(!state.waiter_can_start("scan-1", TaskLane::Scan));
    }

    #[test]
    fn ai_and_scan_can_start_together() {
        let mut state = LaneState::default();
        state.waiters.push_back(WaitEntry {
            task_id: "ai-1".into(),
            lane: TaskLane::Ai,
        });
        state.waiters.push_back(WaitEntry {
            task_id: "scan-1".into(),
            lane: TaskLane::Scan,
        });
        assert!(state.waiter_can_start("ai-1", TaskLane::Ai));
        state.enter(TaskLane::Ai);
        assert!(state.waiter_can_start("scan-1", TaskLane::Scan));
        state.enter(TaskLane::Scan);
        state.leave(TaskLane::Ai);
        state.leave(TaskLane::Scan);
        state.waiters.clear();
        assert!(state.idle_unchecked());
    }

    #[test]
    fn blocking_acquire_waits_for_lane_and_cancel() {
        let bus = TaskBus::new();
        let guard = bus.acquire_immediate(TaskLane::Write).unwrap();
        let handle = TaskHandle::new("queued", TaskKind::Import, "排队");
        let worker = handle.clone();
        let bus_worker = bus.clone();
        let thread = std::thread::spawn(move || {
            bus_worker
                .acquire_blocking(TaskLane::Write, &worker)
                .is_some()
        });
        std::thread::sleep(Duration::from_millis(50));
        drop(guard);
        assert!(thread.join().unwrap());
        assert!(handle.is_finished() || !bus.lane_busy(TaskLane::Write));

        let guard = bus.acquire_immediate(TaskLane::Write).unwrap();
        let handle = TaskHandle::new("cancel-queued", TaskKind::Import, "排队");
        let worker = handle.clone();
        let bus_worker = bus.clone();
        let thread = std::thread::spawn(move || {
            bus_worker
                .acquire_blocking(TaskLane::Write, &worker)
                .is_some()
        });
        std::thread::sleep(Duration::from_millis(50));
        handle.request_cancel();
        bus.inner.ready.notify_all();
        assert!(!thread.join().unwrap());
        drop(guard);
    }
}

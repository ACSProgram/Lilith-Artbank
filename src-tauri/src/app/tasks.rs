//! 统一的后台任务总线。
//!
//! 所有会长时间占用磁盘、写连接或 AI 服务的操作都注册到这里，准入由
//! `concurrency::resources` 的资源表决定：只有真正读写同一资源的任务才互相等待；
//! 长任务冲突时默认按资源 FIFO 排队，路径锁写者优先，短操作仍立即失败。
//! `TaskLane` 只用于任务中心的展示与优先级提示，不再决定冲突。
//! 任务体 panic、线程起不来或提前返回都在这里落到失败终态，并释放资源。
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
    /// 仅用于临时区的 embedding/pose 生成；占用 AI 服务资源。
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
            | Self::Optimize => TaskLane::Write,
            Self::AiEmbedding | Self::AiPose | Self::AiDescription | Self::TempFeatures => {
                TaskLane::Ai
            }
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

    /// 关闭图库时可以直接取消的任务；其余写任务仍要求用户等待完成。
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
    /// 加权进度；任务不提供单位时退回 `completed / total`。
    meter: WorkMeter,
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
            meter: WorkMeter::new(),
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
        if phase == TaskPhase::Completed {
            self.meter.finish();
        }
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
            progress_percent: self.meter.percent(),
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

    fn meter(&self) -> Option<&WorkMeter> {
        Some(&self.meter)
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
        }
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
                if handle.should_cancel() {
                    drop(guard);
                    handle.finish(TaskPhase::Cancelled);
                    return Err(AppError::message("任务已取消"));
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

        if policy == ConflictPolicy::Fail {
            match self.resources.try_acquire(&request, &holder) {
                Ok(guard) => {
                    self.register(handle.clone());
                    spawn_pump(app, handle.clone());
                    handle.set_phase(TaskPhase::Preparing);
                    self.run_immediate(handle.clone(), &name, LaneGuard::from_resource(guard), run);
                    Ok(TaskStartResult::Started {
                        task: handle.snapshot(),
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
                move || match bus
                    .resources
                    .acquire(&request, &holder, None, &|| queued.should_cancel())
                {
                    Ok(guard) => {
                        if queued.should_cancel() {
                            drop(guard);
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

            Ok(TaskStartResult::Started { task: snapshot })
        }
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
}

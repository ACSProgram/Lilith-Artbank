//! 任务总线：注册、排队、资源准入与进度推送。

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tauri::{AppHandle, Emitter};

use crate::{
    app::error::{AppError, AppResult},
    concurrency::{
        budget::ImageBudget,
        resources::{Conflict, HolderInfo, ResourceRequest, ResourceTable, WaitError},
    },
};

use super::{
    ConflictPolicy, EMIT_INTERVAL, MAX_TASKS, TaskPhase, TaskSnapshot, TaskStartResult,
    TaskSummary, fail_from_panic,
    handle::{LaneGuard, TaskHandle, TaskRunGuard, conflict_error, holder_for},
};

#[derive(Default)]
struct BusInner {
    entries: Mutex<VecDeque<Arc<TaskHandle>>>,
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

    pub(super) fn epoch(&self) -> u64 {
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

    pub(super) fn register(&self, handle: Arc<TaskHandle>) {
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

    /// 在当前阻塞线程里跑有返回值的写任务：排队、资源、panic 与终态都走任务总线。
    ///
    /// 调用方必须在阻塞线程里调用。任何退出路径都会落到终态，不会把任务留在排队中。
    pub fn begin_blocking_task<T, F>(
        &self,
        app: &AppHandle,
        handle: Arc<TaskHandle>,
        request: ResourceRequest,
        run: F,
    ) -> AppResult<T>
    where
        F: FnOnce(&TaskHandle) -> AppResult<T>,
    {
        let _run_guard = TaskRunGuard {
            handle: handle.clone(),
        };
        let lane = self.begin_queued(app, handle.clone(), request)?;
        handle.set_phase(TaskPhase::Running);
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&handle))) {
            Ok(result) => {
                handle.finish_from(&result);
                result
            }
            Err(payload) => Err(AppError::message(fail_from_panic(
                &handle,
                "write-task",
                payload.as_ref(),
            ))),
        };
        drop(lane);
        result
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
    pub(super) fn admit_queued(
        &self,
        handle: &TaskHandle,
        stamp: u64,
    ) -> Result<(), Option<&'static str>> {
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

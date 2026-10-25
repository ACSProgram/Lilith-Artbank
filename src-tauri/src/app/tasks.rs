//! 统一的后台任务总线。
//!
//! 所有会长时间占用磁盘、写连接或 AI 服务的操作都注册到这里：
//! 同一时刻只允许一个 `write` 任务；`ai` 与 `scan` 可以并行；同 lane 串行。
//! 进度通过 `task-progress` / `task-finished` 事件推给前端任务中心。

use std::{
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
    library::{
        BatchReporter,
        db::LibraryDb,
        query::{self, LibraryQuery, LibraryView},
        service::now_ms,
    },
};

/// 任务历史保留条数（含已结束），只存在内存里。
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
    ReleaseGarbage,
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
            | Self::ReleaseGarbage => TaskLane::Write,
            Self::AiEmbedding | Self::AiPose | Self::AiDescription => TaskLane::Ai,
            Self::SimilarityGroups => TaskLane::Scan,
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
            Self::ReleaseGarbage => "释放删除文件",
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
    pub message: String,
    pub current_image_id: Option<i64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub failures: Vec<TaskFailure>,
    pub cancellable: bool,
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
    current: Mutex<Option<i64>>,
    message: Mutex<String>,
    failures: Mutex<Vec<TaskFailure>>,
    started_at: i64,
    finished_at: Mutex<Option<i64>>,
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
            current: Mutex::new(None),
            message: Mutex::new(String::new()),
            failures: Mutex::new(Vec::new()),
            started_at: now_ms(),
            finished_at: Mutex::new(None),
        })
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

    pub fn inc_completed(&self) {
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
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
            message: self.message.lock().clone(),
            current_image_id: *self.current.lock(),
            started_at: self.started_at,
            finished_at: *self.finished_at.lock(),
            failures: self.failures.lock().clone(),
            cancellable: self.is_cancellable(),
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

impl BatchReporter for TaskHandle {
    fn report(&self, completed: usize, total: usize) {
        self.set_total(total);
        self.set_completed(completed);
    }

    fn cancelled(&self) -> bool {
        self.should_cancel()
    }
}

#[derive(Default)]
struct LaneState {
    write: usize,
    ai: usize,
    scan: usize,
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
        self.write == 0 && self.ai == 0 && self.scan == 0
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

    pub fn any_busy(&self) -> bool {
        !self.inner.lanes.lock().idle_unchecked()
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
        handle.request_cancel();
        // 排队中的任务也要立刻醒来，看到取消标记后结束。
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
        self.inner.ready.notify_all();
    }

    /// 等待所有任务结束（含排队任务）；超时返回 false。
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let busy = self
                .inner
                .entries
                .lock()
                .iter()
                .any(|handle| handle.is_running());
            if !busy {
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

    /// 排队等待时每 200ms 检查一次取消标记；被取消的排队任务不会继续占用 lane。
    fn acquire_blocking(&self, lane: TaskLane, handle: &TaskHandle) -> Option<LaneGuard> {
        let mut state = self.inner.lanes.lock();
        loop {
            if handle.should_cancel() {
                return None;
            }
            if state.can_start(lane) {
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

    fn register(&self, handle: Arc<TaskHandle>) {
        let mut entries = self.inner.entries.lock();
        entries.push_back(handle);
        while entries.len() > MAX_TASKS {
            entries.pop_front();
        }
    }

    /// 同步占用一个 lane 并开始记录任务（用于命令内部自己跑完的批量操作）。
    pub fn begin_now(&self, app: &AppHandle, handle: Arc<TaskHandle>) -> AppResult<LaneGuard> {
        let guard = self.acquire_immediate(handle.lane())?;
        self.register(handle.clone());
        handle.set_phase(TaskPhase::Preparing);
        spawn_pump(app.clone(), handle);
        Ok(guard)
    }

    /// 在独立线程里执行任务体。线程起不来时把任务标成失败并返回错误，
    /// 否则任务会永远停在“准备中”，还会一直占着 `active()`。
    pub fn spawn_worker<F>(&self, name: String, handle: Arc<TaskHandle>, run: F) -> AppResult<()>
    where
        F: FnOnce() + Send + 'static,
    {
        std::thread::Builder::new()
            .name(name)
            .spawn(run)
            .map(|_| ())
            .map_err(|error| {
                log::error!("无法启动任务线程：{error}");
                handle.set_message(format!("无法启动任务线程：{error}"));
                handle.finish(TaskPhase::Failed);
                AppError::message("无法启动任务线程")
            })
    }

    /// 启动任务：lane 空闲时立即运行；冲突时按 policy 失败、排队或取消现有任务。
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
        let active = self.conflicting(handle.lane());
        if !active.is_empty() {
            match policy {
                ConflictPolicy::Fail => return Ok(TaskStartResult::Conflict { active }),
                ConflictPolicy::CancelActive => {
                    for task in &active {
                        let _ = self.cancel(&task.task_id);
                    }
                }
                ConflictPolicy::Queue => {}
            }
        }

        self.register(handle.clone());
        spawn_pump(app, handle.clone());

        if active.is_empty()
            && let Ok(guard) = self.acquire_immediate(handle.lane())
        {
            handle.set_phase(TaskPhase::Preparing);
            run(guard);
            return Ok(TaskStartResult::Started {
                task: handle.snapshot(),
            });
        }

        handle.set_phase(TaskPhase::Queued);
        handle.set_message("排队等待前一个任务结束");
        let snapshot = handle.snapshot();
        let bus = self.clone();
        let queued = handle.clone();
        self.spawn_worker(format!("task-queue-{}", queued.id()), handle, move || {
            let Some(guard) = bus.acquire_blocking(queued.lane(), &queued) else {
                queued.finish(TaskPhase::Cancelled);
                return;
            };
            if queued.should_cancel() {
                drop(guard);
                queued.finish(TaskPhase::Cancelled);
                return;
            }
            queued.set_message("");
            queued.set_phase(TaskPhase::Preparing);
            run(guard);
        })?;

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
    /// lane 被占用时返回正在运行的任务，由前端决定排队还是取消现有任务。
    Conflict {
        active: Vec<TaskSummary>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    #[default]
    Fail,
    Queue,
    CancelActive,
}

/// 前端只传语义范围，id 集合在 Rust 侧解析，避免几十万 id 过 IPC。
#[derive(Debug, Clone, Deserialize, Default)]
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
        assert_eq!(TaskKind::ReleaseGarbage.lane(), TaskLane::Write);
        assert_eq!(TaskKind::Undo.label(), "撤销操作");
        assert_eq!(TaskKind::ReleaseGarbage.label(), "释放删除文件");
    }
}

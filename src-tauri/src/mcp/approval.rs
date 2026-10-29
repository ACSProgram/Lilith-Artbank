//! 询问档的待确认队列。
//!
//! 工具调用只登记一条待确认请求，不立即执行；用户在应用内批准后才真正调用领域函数。
//! 待确认请求 10 分钟无人处理自动过期。队列只存在内存里，应用退出即丢弃。
//!
//! 执行中的记录不自动判失败：任务进度 10 分钟没有变化只标“疑似卡住”，
//! 当前进度照样返回给 AI，由它决定继续等还是取消。

use std::{collections::VecDeque, sync::Arc, time::Duration};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};

use crate::app::{
    error::{AppError, AppResult},
    tasks::{TaskPhase, TaskSnapshot},
};

/// 待确认请求保留多久；过期后 AI 再查询会看到 `expired`。
pub const APPROVAL_TTL: Duration = Duration::from_secs(10 * 60);
/// 执行中的记录多久没有进度算“疑似卡住”；只做标注，不改状态。
pub const STALLED_AFTER: Duration = Duration::from_secs(10 * 60);
const MAX_APPROVALS: usize = 50;
pub const APPROVAL_EVENT: &str = "mcp-approval-changed";

/// 无头验收用 `LILITH_MCP_APPROVAL_TTL_MS` 把确认有效期缩到秒级；GUI 构建固定 10 分钟。
pub fn ttl() -> Duration {
    #[cfg(feature = "headless")]
    if let Ok(value) = std::env::var("LILITH_MCP_APPROVAL_TTL_MS")
        && let Ok(millis) = value.trim().parse::<u64>()
        && millis > 0
    {
        return Duration::from_millis(millis);
    }
    APPROVAL_TTL
}

/// 无头验收用 `LILITH_MCP_APPROVAL_STALL_MS` 把“疑似卡住”的判定缩到秒级。
pub fn stalled_after() -> Duration {
    #[cfg(feature = "headless")]
    if let Ok(value) = std::env::var("LILITH_MCP_APPROVAL_STALL_MS")
        && let Ok(millis) = value.trim().parse::<u64>()
        && millis > 0
    {
        return Duration::from_millis(millis);
    }
    STALLED_AFTER
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalStatus {
    Pending,
    Running,
    Completed,
    Rejected,
    Expired,
    Failed,
}

impl ApprovalStatus {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// 批准后要执行的工具名与原始参数；执行前会重新解析 selector。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalAction {
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ApprovalRecord {
    pub id: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub status: ApprovalStatus,
    pub title: String,
    pub action: ApprovalAction,
    pub preview: Value,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub task_id: Option<String>,
    /// 执行中最后一次看到进度的时刻；`begin` 时置为当前时间。
    pub last_progress_at: i64,
    /// 最后一次看到的进度摘要，任务快照被回收后仍能回答“做到哪了”。
    pub progress: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalView {
    pub id: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub status: ApprovalStatus,
    pub title: String,
    pub tool: String,
    pub preview: Value,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub task_id: Option<String>,
    /// 执行中且超过 10 分钟没有进度：只标注，不改状态，由 AI 决定继续等还是取消。
    pub stalled: bool,
    pub progress: Option<Value>,
}

impl ApprovalRecord {
    pub fn view(&self, now: i64) -> ApprovalView {
        ApprovalView {
            id: self.id.clone(),
            created_at: self.created_at,
            expires_at: self.expires_at,
            status: self.status,
            title: self.title.clone(),
            tool: self.action.tool.clone(),
            preview: self.preview.clone(),
            result: self.result.clone(),
            error: self.error.clone(),
            task_id: self.task_id.clone(),
            stalled: self.is_stalled(now),
            progress: self.progress.clone(),
        }
    }

    fn is_stalled(&self, now: i64) -> bool {
        self.status == ApprovalStatus::Running
            && now.saturating_sub(self.last_progress_at) >= stalled_after().as_millis() as i64
    }
}

#[derive(Clone)]
pub struct ApprovalQueue {
    app: Option<AppHandle>,
    entries: Arc<Mutex<VecDeque<ApprovalRecord>>>,
}

impl ApprovalQueue {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app: Some(app),
            entries: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    #[cfg(test)]
    fn detached() -> Self {
        Self {
            app: None,
            entries: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// 登记一条待确认请求；返回给 AI 的 `approvalId`。
    pub fn enqueue(
        &self,
        title: impl Into<String>,
        action: ApprovalAction,
        preview: Value,
    ) -> ApprovalView {
        let now = chrono::Local::now().timestamp_millis();
        let record = ApprovalRecord {
            id: format!("approval-{}", uuid::Uuid::new_v4().simple()),
            created_at: now,
            expires_at: now + ttl().as_millis() as i64,
            status: ApprovalStatus::Pending,
            title: title.into(),
            action,
            preview,
            result: None,
            error: None,
            task_id: None,
            last_progress_at: now,
            progress: None,
        };
        let view = record.view(now);
        {
            let mut entries = self.entries.lock();
            expire_locked(&mut entries, now);
            entries.push_back(record);
            trim_locked(&mut entries);
        }
        self.emit_changed();
        view
    }

    pub fn list(&self) -> Vec<ApprovalView> {
        let now = chrono::Local::now().timestamp_millis();
        let mut entries = self.entries.lock();
        expire_locked(&mut entries, now);
        entries.iter().rev().map(|entry| entry.view(now)).collect()
    }

    /// 用任务快照推进执行中的记录：有进度就刷新时间，进入终态就回写状态。
    ///
    /// 回写是必要的：任务快照只保留最近 50 条，记录不能因为任务被回收就永远停在 `running`。
    /// 只有状态变化才发事件——进度刷新会被 `tasks_get` 的轮询反复触发，发事件会把界面刷成风暴。
    pub fn sync_tasks(&self, tasks: &[TaskSnapshot]) {
        let now = chrono::Local::now().timestamp_millis();
        let mut settled = false;
        {
            let mut entries = self.entries.lock();
            for entry in entries.iter_mut() {
                if entry.status != ApprovalStatus::Running {
                    continue;
                }
                let Some(task_id) = entry.task_id.as_deref() else {
                    continue;
                };
                let Some(task) = tasks.iter().find(|task| task.task_id == task_id) else {
                    continue;
                };
                match task.phase {
                    TaskPhase::Completed => {
                        entry.status = ApprovalStatus::Completed;
                        settled = true;
                    }
                    TaskPhase::PartialFailed | TaskPhase::Failed => {
                        entry.status = ApprovalStatus::Failed;
                        if entry.error.is_none() {
                            entry.error = Some(task.message.clone());
                        }
                        settled = true;
                    }
                    TaskPhase::Cancelled => {
                        entry.status = ApprovalStatus::Failed;
                        entry.error = Some("任务已取消".to_owned());
                        settled = true;
                    }
                    _ => {
                        let progress = task_progress(task);
                        if entry.progress.as_ref() != Some(&progress) {
                            entry.progress = Some(progress);
                            entry.last_progress_at = now;
                        }
                    }
                }
            }
        }
        if settled {
            self.emit_changed();
        }
    }

    /// 换库或关库时调用：待确认请求绑定的是旧图库的 selector，不能带到新库执行。
    pub fn clear(&self) {
        self.entries.lock().clear();
        self.emit_changed();
    }

    /// 批准前取走动作并把状态改成 `running`；这里不执行，执行由调用方完成。
    pub fn begin(&self, id: &str) -> AppResult<ApprovalRecord> {
        let record = {
            let mut entries = self.entries.lock();
            expire_locked(&mut entries, chrono::Local::now().timestamp_millis());
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| AppError::message("待确认操作不存在或已过期"))?;
            match entry.status {
                ApprovalStatus::Pending => {}
                ApprovalStatus::Expired => {
                    return Err(AppError::message("待确认操作已过期，请让 AI 重新发起"));
                }
                ApprovalStatus::Rejected => {
                    return Err(AppError::message("这条待确认操作已被拒绝"));
                }
                _ => return Err(AppError::message("这条待确认操作已经开始处理")),
            }
            entry.status = ApprovalStatus::Running;
            entry.last_progress_at = chrono::Local::now().timestamp_millis();
            entry.progress = None;
            entry.clone()
        };
        self.emit_changed();
        Ok(record)
    }

    pub fn complete(&self, id: &str, result: Value) {
        self.update(id, |entry| {
            entry.status = ApprovalStatus::Completed;
            entry.result = Some(result);
            entry.error = None;
        });
    }

    pub fn attach_task(&self, id: &str, task_id: &str, result: Value) {
        let now = chrono::Local::now().timestamp_millis();
        self.update(id, |entry| {
            entry.status = ApprovalStatus::Running;
            entry.task_id = Some(task_id.to_owned());
            entry.result = Some(result);
            entry.last_progress_at = now;
        });
    }

    pub fn fail(&self, id: &str, error: impl Into<String>) {
        self.fail_with(id, error, None);
    }

    /// 执行前复核失败时把最新的预览一起留下，AI 轮询确认单时能直接看到新数量。
    pub fn fail_with(&self, id: &str, error: impl Into<String>, result: Option<Value>) {
        self.update(id, |entry| {
            entry.status = ApprovalStatus::Failed;
            entry.error = Some(error.into());
            if result.is_some() {
                entry.result = result;
            }
        });
    }

    pub fn reject(&self, id: &str) -> AppResult<()> {
        let changed = {
            let mut entries = self.entries.lock();
            expire_locked(&mut entries, chrono::Local::now().timestamp_millis());
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| AppError::message("待确认操作不存在或已过期"))?;
            if entry.status != ApprovalStatus::Pending {
                return Err(AppError::message("这条待确认操作已经开始处理或已结束"));
            }
            entry.status = ApprovalStatus::Rejected;
            entry.error = Some("用户拒绝了这次操作".to_owned());
            true
        };
        if changed {
            self.emit_changed();
        }
        Ok(())
    }

    fn update(&self, id: &str, apply: impl FnOnce(&mut ApprovalRecord)) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
                apply(entry);
            }
        }
        self.emit_changed();
    }

    fn emit_changed(&self) {
        let Some(app) = &self.app else {
            return;
        };
        let (open, total) = {
            let entries = self.entries.lock();
            (
                entries
                    .iter()
                    .filter(|entry| entry.status.is_open())
                    .count(),
                entries.len(),
            )
        };
        let _ = app.emit(
            APPROVAL_EVENT,
            serde_json::json!({ "pending": open, "total": total }),
        );
    }
}

fn expire_locked(entries: &mut VecDeque<ApprovalRecord>, now: i64) {
    for entry in entries.iter_mut() {
        if entry.status == ApprovalStatus::Pending && entry.expires_at <= now {
            entry.status = ApprovalStatus::Expired;
            entry.error = Some("等待确认超时，操作已取消".to_owned());
        }
    }
}

/// 超过上限时的淘汰顺序：先丢终态记录，再丢最老的待确认单。
///
/// 执行中的记录一条都不能丢：AI 手里还有 `approvalId`，丢掉会让 `tasks_get` 回答“不存在”，
/// 而且它背后真的有一个任务在跑。
fn trim_locked(entries: &mut VecDeque<ApprovalRecord>) {
    while entries.len() > MAX_APPROVALS {
        if let Some(index) = entries.iter().position(|entry| !entry.status.is_open()) {
            entries.remove(index);
            continue;
        }
        // 全是未终态：只淘汰最老的待确认单（它本来就有 10 分钟有效期）。
        let Some(index) = entries
            .iter()
            .position(|entry| entry.status == ApprovalStatus::Pending)
        else {
            log::warn!("MCP 待确认队列里没有可淘汰的记录，临时超出上限 {MAX_APPROVALS}");
            return;
        };
        entries.remove(index);
        log::warn!("MCP 待确认队列已满，最老的一条待确认请求被丢弃");
    }
}

fn task_progress(task: &TaskSnapshot) -> Value {
    json!({
        "phase": task.phase,
        "total": task.total,
        "completed": task.completed,
        "failed": task.failed,
        "skipped": task.skipped,
        "message": task.message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action() -> ApprovalAction {
        ApprovalAction {
            tool: "images_trash".into(),
            arguments: serde_json::json!({ "targetId": 1 }),
        }
    }

    #[test]
    fn approval_moves_through_pending_running_and_completed() {
        let queue = ApprovalQueue::detached();
        let view = queue.enqueue("移入回收站", action(), serde_json::json!({ "count": 2 }));
        assert_eq!(view.status, ApprovalStatus::Pending);
        assert_eq!(queue.list().len(), 1);

        let record = queue.begin(&view.id).unwrap();
        assert_eq!(record.status, ApprovalStatus::Running);
        assert!(queue.begin(&view.id).is_err(), "重复批准应被拒绝");

        queue.complete(&view.id, serde_json::json!({ "status": "ok" }));
        let view = queue
            .list()
            .into_iter()
            .find(|entry| entry.id == view.id)
            .unwrap();
        assert_eq!(view.status, ApprovalStatus::Completed);
        assert_eq!(view.result.unwrap()["status"], "ok");
    }

    #[test]
    fn approval_can_be_rejected_only_while_pending() {
        let queue = ApprovalQueue::detached();
        let first = queue.enqueue("拒绝", action(), Value::Null);
        queue.reject(&first.id).unwrap();
        assert!(queue.reject(&first.id).is_err());
        let rejected = queue
            .list()
            .into_iter()
            .find(|entry| entry.id == first.id)
            .unwrap();
        assert_eq!(rejected.status, ApprovalStatus::Rejected);

        let second = queue.enqueue("开始后不能拒绝", action(), Value::Null);
        queue.begin(&second.id).unwrap();
        assert!(queue.reject(&second.id).is_err());
    }

    #[test]
    fn pending_approval_expires_and_cannot_start() {
        let queue = ApprovalQueue::detached();
        let view = queue.enqueue("过期", action(), Value::Null);
        {
            let mut entries = queue.entries.lock();
            entries[0].expires_at = 0;
        }
        let expired = queue
            .list()
            .into_iter()
            .find(|entry| entry.id == view.id)
            .unwrap();
        assert_eq!(expired.status, ApprovalStatus::Expired);
        assert!(queue.begin(&view.id).is_err());
    }

    #[test]
    fn failed_execution_keeps_latest_preview() {
        let queue = ApprovalQueue::detached();
        let view = queue.enqueue("复核失败", action(), Value::Null);
        queue.begin(&view.id).unwrap();
        queue.fail_with(
            &view.id,
            "数量变化",
            Some(serde_json::json!({ "latest": { "selected": 3 } })),
        );
        let view = queue
            .list()
            .into_iter()
            .find(|entry| entry.id == view.id)
            .unwrap();
        assert_eq!(view.status, ApprovalStatus::Failed);
        assert_eq!(view.error.as_deref(), Some("数量变化"));
        assert_eq!(view.result.unwrap()["latest"]["selected"], 3);
    }

    // ApprovalQueue 的过期判定也可以脱离 AppHandle 单独验证。
    #[test]
    fn expiry_only_touches_pending_entries() {
        let mut entries = VecDeque::from([
            record("a", ApprovalStatus::Pending),
            record("b", ApprovalStatus::Completed),
        ]);
        expire_locked(&mut entries, 11);
        assert_eq!(entries[0].status, ApprovalStatus::Expired);
        assert_eq!(entries[1].status, ApprovalStatus::Completed);
    }

    /// 队列满时先淘汰终态记录；执行中的记录不能因为来了新请求就消失。
    #[test]
    fn trimming_never_drops_a_running_approval() {
        let mut entries = VecDeque::new();
        entries.push_back(record("running", ApprovalStatus::Running));
        for index in 0..MAX_APPROVALS {
            entries.push_back(record(&format!("pending-{index}"), ApprovalStatus::Pending));
        }
        trim_locked(&mut entries);
        assert_eq!(entries.len(), MAX_APPROVALS);
        assert!(
            entries.iter().any(|entry| entry.id == "running"),
            "执行中的记录不能被淘汰"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.status == ApprovalStatus::Pending)
                .count(),
            MAX_APPROVALS - 1
        );

        // 有终态记录时优先丢终态，待确认单一个都不动。
        entries.push_back(record("done", ApprovalStatus::Completed));
        entries.push_back(record("pending-new", ApprovalStatus::Pending));
        trim_locked(&mut entries);
        assert_eq!(entries.len(), MAX_APPROVALS);
        assert!(!entries.iter().any(|entry| entry.id == "done"));
        assert!(entries.iter().any(|entry| entry.id == "pending-new"));
    }

    /// 执行中 10 分钟没有进度只标“疑似卡住”，状态仍是 running，进度照旧可读。
    #[test]
    fn running_approval_is_marked_stalled_instead_of_failing() {
        let queue = ApprovalQueue::detached();
        let view = queue.enqueue("长任务", action(), Value::Null);
        queue.begin(&view.id).unwrap();
        queue.attach_task(&view.id, "task-1", json!({ "status": "queued" }));
        {
            let mut entries = queue.entries.lock();
            entries[0].last_progress_at =
                chrono::Local::now().timestamp_millis() - stalled_after().as_millis() as i64 - 1;
            entries[0].progress = Some(json!({ "completed": 3, "total": 10 }));
        }
        let view = queue
            .list()
            .into_iter()
            .find(|entry| entry.id == view.id)
            .unwrap();
        assert_eq!(view.status, ApprovalStatus::Running);
        assert!(view.stalled, "超时没有进度应标疑似卡住");
        assert_eq!(view.progress.unwrap()["completed"], 3);
    }

    /// 任务进入终态要回写记录：任务快照被回收后，记录不能永远停在 running。
    #[test]
    fn task_terminal_state_is_written_back() {
        let queue = ApprovalQueue::detached();
        let view = queue.enqueue("导入", action(), Value::Null);
        queue.begin(&view.id).unwrap();
        let handle = crate::app::tasks::TaskHandle::new(
            "task-1",
            crate::app::tasks::TaskKind::Import,
            "范围",
        );
        queue.attach_task(&view.id, "task-1", json!({ "status": "queued" }));

        handle.set_total(4);
        handle.set_phase(crate::app::tasks::TaskPhase::Running);
        handle.inc_completed();
        queue.sync_tasks(&[handle.snapshot()]);
        let running = queue
            .list()
            .into_iter()
            .find(|entry| entry.id == view.id)
            .unwrap();
        assert_eq!(running.status, ApprovalStatus::Running);
        assert_eq!(running.progress.unwrap()["completed"], 1);
        assert!(!running.stalled);

        handle.set_phase(crate::app::tasks::TaskPhase::Completed);
        queue.sync_tasks(&[handle.snapshot()]);
        let done = queue
            .list()
            .into_iter()
            .find(|entry| entry.id == view.id)
            .unwrap();
        assert_eq!(done.status, ApprovalStatus::Completed);
        assert!(!done.stalled);
    }

    fn record(id: &str, status: ApprovalStatus) -> ApprovalRecord {
        ApprovalRecord {
            id: id.into(),
            created_at: 0,
            expires_at: 10,
            status,
            title: String::new(),
            action: ApprovalAction {
                tool: "images_trash".into(),
                arguments: Value::Null,
            },
            preview: Value::Null,
            result: None,
            error: None,
            task_id: None,
            last_progress_at: 0,
            progress: None,
        }
    }
}

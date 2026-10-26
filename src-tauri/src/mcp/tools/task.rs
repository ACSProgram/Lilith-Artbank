//! MCP 任务查询与取消。
//!
//! `tasks_get` 可以等任务或待确认操作进入终态（单次最多 30 秒）；不带 id 时返回活动任务、
//! 最近结束的任务与待确认列表。导入任务额外附带回执（统计、预览 id 与失败清单）。
//! `tasks_cancel` 只能取消 MCP 自己发起的任务或拒绝自己的确认单。

use std::time::{Duration, Instant};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{
        error::{AppError, AppResult},
        state::AppState,
        tasks::{TaskKind, TaskPhase, TaskSnapshot},
    },
    import::job::ImportSnapshot,
    mcp::approval::ApprovalView,
};

use super::ToolOutcome;

const MAX_WAIT_MS: u64 = 30_000;
const RECENT_FINISHED: usize = 20;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct TasksGetInput {
    #[schemars(description = "任务 id 或待确认 approvalId；省略时返回活动任务与最近结束的任务")]
    pub task_id: Option<String>,
    #[schemars(description = "最多等待多少毫秒，0-30000；任务已进入终态时立即返回")]
    pub wait_ms: Option<u64>,
    #[schemars(description = "是否附带失败清单；默认 false")]
    pub include_failures: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TasksCancelInput {
    #[schemars(description = "任务 id；传 approvalId 等同于拒绝这次待确认操作")]
    pub task_id: String,
}

pub fn tasks_get(state: &AppState, input: &TasksGetInput) -> AppResult<ToolOutcome> {
    let wait_ms = input.wait_ms.unwrap_or(0).min(MAX_WAIT_MS);
    let include_failures = input.include_failures.unwrap_or(false);
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let Some(id) = input
        .task_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Ok(ToolOutcome::value(list_value(state, include_failures)));
    };
    if id.starts_with("approval-") {
        return Ok(ToolOutcome::value(wait_approval(
            state,
            id,
            deadline,
            include_failures,
        )?));
    }
    let snapshot = wait_task(state, id, deadline)?;
    Ok(ToolOutcome::value(task_value(
        state,
        &snapshot,
        include_failures,
        wait_ms > 0,
    )))
}

pub fn tasks_cancel(state: &AppState, input: &TasksCancelInput) -> AppResult<ToolOutcome> {
    if input.task_id.starts_with("approval-") {
        state.reject_mcp_approval(&input.task_id)?;
        return Ok(ToolOutcome::value(json!({
            "status": "rejected",
            "approvalId": input.task_id,
        })));
    }
    if !state.is_mcp_task(&input.task_id) {
        return Err(AppError::message(
            "只能取消 MCP 自己发起的任务；用户从界面启动的任务不能由 AI 取消",
        ));
    }
    state.cancel_task(&input.task_id)?;
    Ok(ToolOutcome::value(json!({
        "status": "cancelled",
        "taskId": input.task_id,
    })))
}

fn list_value(state: &AppState, include_failures: bool) -> Value {
    let tasks = state.tasks.list();
    let active = tasks
        .iter()
        .filter(|task| !task.phase.is_terminal())
        .map(|task| task_value(state, task, include_failures, false))
        .collect::<Vec<_>>();
    let recent = tasks
        .iter()
        .filter(|task| task.phase.is_terminal())
        .take(RECENT_FINISHED)
        .map(|task| task_value(state, task, include_failures, false))
        .collect::<Vec<_>>();
    let approvals = state
        .mcp_approval_views()
        .into_iter()
        .filter(|approval| approval.status.is_open())
        .map(|approval| approval_value(&approval))
        .collect::<Vec<_>>();
    json!({
        "active": active,
        "recent": recent,
        "pendingApprovals": approvals,
        "note": "写入被拒绝时先看 active；重做分支会被新的写入清空。",
    })
}

fn task_value(
    state: &AppState,
    snapshot: &TaskSnapshot,
    include_failures: bool,
    waited: bool,
) -> Value {
    let mut value = serde_json::to_value(snapshot)
        .unwrap_or_else(|error| json!({ "error": format!("任务快照序列化失败：{error}") }));
    let omitted = !include_failures && !snapshot.failures.is_empty();
    if let Value::Object(object) = &mut value {
        if !include_failures {
            object.insert("failures".to_owned(), Value::Array(Vec::new()));
        }
        if omitted {
            object.insert("failuresOmitted".to_owned(), Value::Bool(true));
            object.insert("failureCount".to_owned(), json!(snapshot.failures.len()));
        }
        if waited {
            object.insert(
                "waitTimedOut".to_owned(),
                Value::Bool(!snapshot.phase.is_terminal()),
            );
        }
        if snapshot.kind == TaskKind::Import
            && let Some(receipt) = state.import_receipt(&snapshot.task_id)
        {
            object.insert(
                "import".to_owned(),
                import_receipt_value(&receipt, include_failures),
            );
        }
    }
    value
}

/// 导入回执只给统计、预览与失败；不返回压缩字段，避免让 AI 以为可以控制压缩。
fn import_receipt_value(receipt: &ImportSnapshot, include_failures: bool) -> Value {
    let progress = &receipt.progress;
    let failure_count = receipt.failures.len();
    let failures = if include_failures {
        json!(receipt.failures)
    } else {
        Value::Array(Vec::new())
    };
    json!({
        "phase": progress.phase,
        "target": progress.target,
        "total": progress.total,
        "processed": progress.processed,
        "imported": progress.imported,
        "duplicates": progress.duplicates,
        "merged": progress.merged,
        "failed": progress.failed,
        "similar": progress.similar,
        "previewIds": receipt.preview_ids,
        "failures": failures,
        "failureCount": failure_count,
        "startedAt": receipt.started_at,
        "finishedAt": receipt.finished_at,
    })
}

fn approval_value(view: &ApprovalView) -> Value {
    serde_json::to_value(view)
        .unwrap_or_else(|error| json!({ "error": format!("确认单序列化失败：{error}") }))
}

fn wait_task(state: &AppState, id: &str, deadline: Instant) -> AppResult<TaskSnapshot> {
    loop {
        let snapshot = state
            .tasks
            .list()
            .into_iter()
            .find(|task| task.task_id == id);
        match snapshot {
            Some(snapshot) if snapshot.phase.is_terminal() || Instant::now() >= deadline => {
                return Ok(snapshot);
            }
            Some(_) => std::thread::sleep(POLL_INTERVAL),
            None => {
                return Err(AppError::message(
                    "任务不存在或已结束（任务列表只保留最近 50 条）",
                ));
            }
        }
    }
}

fn wait_approval(
    state: &AppState,
    id: &str,
    deadline: Instant,
    include_failures: bool,
) -> AppResult<Value> {
    loop {
        let view = state
            .mcp_approval_views()
            .into_iter()
            .find(|approval| approval.id == id)
            .ok_or_else(|| AppError::message("待确认操作不存在或已过期"))?;
        let task = view.task_id.as_deref().and_then(|task_id| {
            state
                .tasks
                .list()
                .into_iter()
                .find(|task| task.task_id == task_id)
        });
        let settled = !view.status.is_open()
            || task
                .as_ref()
                .is_some_and(|task| task.phase == TaskPhase::Cancelled);
        let timed_out = Instant::now() >= deadline;
        if settled || timed_out {
            let mut value = json!({
                "approval": approval_value(&view),
                "task": task
                    .as_ref()
                    .map(|task| task_value(state, task, include_failures, false)),
            });
            if let Value::Object(object) = &mut value {
                object.insert("waitTimedOut".to_owned(), Value::Bool(!settled));
            }
            return Ok(value);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{ImportTarget, job::ImportJob};

    #[test]
    fn import_receipt_reports_counts_and_hides_compression() {
        let job = ImportJob::new("job-1", "1 个路径", ImportTarget::Library);
        job.set_total(10);
        job.inc_processed();
        job.inc_imported(3);
        job.inc_duplicates(2);
        job.inc_merged(1);
        job.inc_failed();
        job.inc_optimized(4, 1024);
        job.add_preview(7);
        job.add_preview(8);
        job.add_failure("a.png", "无法解码");

        let value = import_receipt_value(&job.snapshot(), false);
        assert_eq!(value["total"], 10);
        assert_eq!(value["imported"], 3);
        assert_eq!(value["duplicates"], 2);
        assert_eq!(value["merged"], 1);
        assert_eq!(value["failed"], 1);
        assert_eq!(value["previewIds"], json!([7, 8]));
        assert_eq!(value["failureCount"], 1);
        assert_eq!(value["failures"], json!([]));
        assert!(value.get("optimized").is_none());
        assert!(value.get("optimizedBytes").is_none());
    }

    #[test]
    fn import_receipt_can_include_failures() {
        let job = ImportJob::new("job-2", "1 个路径", ImportTarget::Library);
        job.add_failure("a.png", "无法解码");
        let value = import_receipt_value(&job.snapshot(), true);
        assert_eq!(value["failures"][0]["path"], "a.png");
        assert_eq!(value["failures"][0]["reason"], "无法解码");
    }
}

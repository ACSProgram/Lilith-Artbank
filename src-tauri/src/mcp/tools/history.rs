//! MCP 操作记录工具：列表、撤销与重做。
//!
//! 撤销/重做永远先给出回退清单并核对 `confirmedSteps` / `confirmedAffected`；
//! 询问档还要用户批准。压缩记录阻断撤销时，原样返回领域层的错误，不自动改目标或重试。

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::McpMode,
        state::AppState,
        tasks::{TaskStartResult, TaskSummary},
    },
    library::history::{self, HistoryEntry, HistoryKind, OperationPreview, OperationStatus},
    mcp::approval::ApprovalAction,
};

use super::ToolOutcome;

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryListInput {
    #[schemars(description = "最多返回条数，1-500；默认 100")]
    pub limit: Option<u32>,
    #[schemars(description = "只列某种记录，例如 trash、score、tagAdd、collectionAdd、optimize")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRunInput {
    #[schemars(description = "记录 id；撤销目标必须是 applied，重做目标必须是 undone")]
    pub target_id: i64,
    #[schemars(description = "dryRun 返回的 steps；正式执行必须一致")]
    #[serde(default)]
    pub confirmed_steps: Option<u64>,
    #[schemars(description = "dryRun 返回的 affected；正式执行必须一致")]
    #[serde(default)]
    pub confirmed_affected: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

pub fn history_list(state: &AppState, input: &HistoryListInput) -> AppResult<ToolOutcome> {
    let page = state.history_page()?;
    let mode = state.settings.snapshot().mcp.mode;
    let read_only = mode == McpMode::ReadOnly;
    let can_run = !read_only && page.can_run;
    let blocked_reason = if read_only {
        Some("当前是只读模式，不能撤销或重做".to_owned())
    } else {
        page.blocked_reason.clone()
    };
    let limit = input.limit.unwrap_or(100).clamp(1, 500) as usize;
    let kind = input
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| !kind.is_empty());
    let all = &page.entries;
    let entries: Vec<Value> = all
        .iter()
        .enumerate()
        .filter(|(_, entry)| kind.is_none_or(|kind| entry.kind.as_str() == kind))
        .take(limit)
        .map(|(index, entry)| decorate_entry(all, index, entry, can_run, &blocked_reason))
        .collect();
    // 顶层 canRun 只表示“返回的条目里真有一条能执行”，具体看条目的 canUndo/canRedo。
    let actionable = entries.iter().any(|entry| {
        entry["canUndo"] == Value::Bool(true) || entry["canRedo"] == Value::Bool(true)
    });
    let top_reason = if actionable {
        None
    } else if !can_run {
        blocked_reason.clone()
    } else {
        Some("返回的记录里没有可撤销或可重做的条目".to_owned())
    };
    Ok(ToolOutcome::value(json!({
        "entries": entries,
        "count": entries.len(),
        "total": all.len(),
        "maxSteps": page.max_steps,
        "mode": mode.as_str(),
        "canRun": actionable,
        "blockedReason": top_reason,
        "pendingPurgeCount": page.pending_purge_count,
        "pendingPurgeBytes": page.pending_purge_bytes,
        "note": "新写入会清空灰色重做分支；撤销链包含压缩记录时会被明确拒绝。",
    })))
}

fn decorate_entry(
    all: &[HistoryEntry],
    index: usize,
    entry: &HistoryEntry,
    can_run: bool,
    blocked: &Option<String>,
) -> Value {
    let undo_chain: Vec<&HistoryEntry> = all
        .iter()
        .filter(|other| {
            other.id >= entry.id
                && matches!(
                    other.status,
                    OperationStatus::Applied | OperationStatus::Undoing
                )
        })
        .collect();
    let redo_chain: Vec<&HistoryEntry> = all[index..]
        .iter()
        .take_while(|other| {
            matches!(
                other.status,
                OperationStatus::Undone | OperationStatus::Redoing
            )
        })
        .collect();
    let undo_chain_blocked = undo_chain
        .iter()
        .any(|other| other.kind == HistoryKind::Optimize);
    let redo_chain_blocked = redo_chain
        .iter()
        .any(|other| other.kind == HistoryKind::Optimize);
    let newer_failure = all.iter().any(|other| {
        other.id > entry.id
            && (other.status == OperationStatus::UndoFailed
                || (matches!(
                    other.status,
                    OperationStatus::Undone | OperationStatus::Redoing
                ) && !other.error.is_empty()))
    });

    let undo_steps = if entry.status == OperationStatus::Applied {
        undo_chain.len() as u64
    } else {
        0
    };
    let redo_steps = if matches!(
        entry.status,
        OperationStatus::Undone | OperationStatus::Redoing
    ) {
        redo_chain.len() as u64
    } else {
        0
    };
    let can_undo = can_run
        && entry.status == OperationStatus::Applied
        && !newer_failure
        && !undo_chain_blocked;
    let can_redo = can_run
        && matches!(
            entry.status,
            OperationStatus::Undone | OperationStatus::Redoing
        )
        && !redo_chain_blocked;
    let blocked_reason = if entry.status == OperationStatus::UndoFailed {
        Some("这条记录上次撤销失败，需要先处理失败原因".to_owned())
    } else if entry.status == OperationStatus::UndoSkipped {
        Some("这条记录已跳过撤销".to_owned())
    } else if (entry.status == OperationStatus::Applied && undo_chain_blocked)
        || (matches!(
            entry.status,
            OperationStatus::Undone | OperationStatus::Redoing
        ) && redo_chain_blocked)
    {
        Some("压缩操作不可撤销，原图已删除".to_owned())
    } else if newer_failure && entry.status == OperationStatus::Applied {
        Some("之后有一条记录撤销或重做失败，不能继续回退".to_owned())
    } else if !can_run {
        blocked.clone()
    } else if entry.status == OperationStatus::Applied && undo_steps == 0 {
        Some("没有可撤销的记录".to_owned())
    } else if matches!(
        entry.status,
        OperationStatus::Undone | OperationStatus::Redoing
    ) && redo_steps == 0
    {
        Some("没有可重做的记录".to_owned())
    } else {
        None
    };

    json!({
        "id": entry.id,
        "kind": entry.kind,
        "label": entry.label,
        "createdAt": entry.created_at,
        "affected": entry.affected,
        "status": entry.status,
        "error": entry.error,
        "undoSteps": undo_steps,
        "redoSteps": redo_steps,
        "canUndo": can_undo,
        "canRedo": can_redo,
        "blockedReason": blocked_reason,
    })
}

pub fn history_undo(state: &AppState, input: &HistoryRunInput) -> AppResult<ToolOutcome> {
    run_history(state, input, RunKind::Undo, false)
}

pub fn execute_undo(state: &AppState, input: &HistoryRunInput) -> AppResult<ToolOutcome> {
    run_history(state, input, RunKind::Undo, true)
}

pub fn history_redo(state: &AppState, input: &HistoryRunInput) -> AppResult<ToolOutcome> {
    run_history(state, input, RunKind::Redo, false)
}

pub fn execute_redo(state: &AppState, input: &HistoryRunInput) -> AppResult<ToolOutcome> {
    run_history(state, input, RunKind::Redo, true)
}

#[derive(Clone, Copy)]
enum RunKind {
    Undo,
    Redo,
}

impl RunKind {
    fn tool(self) -> &'static str {
        match self {
            Self::Undo => "history_undo",
            Self::Redo => "history_redo",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Undo => "撤销",
            Self::Redo => "重做",
        }
    }
}

fn preview(state: &AppState, kind: RunKind, target_id: i64) -> AppResult<OperationPreview> {
    let (database, _) = state
        .current_library()
        .ok_or_else(|| AppError::message("尚未打开图库"))?;
    match kind {
        RunKind::Undo => history::preview_undo(&database, target_id),
        RunKind::Redo => history::preview_redo(&database, target_id),
    }
}

fn preview_value(preview: &OperationPreview, kind: RunKind) -> Value {
    json!({
        "targetId": preview.target_id,
        "steps": preview.steps,
        "affected": preview.affected,
        "entries": preview.entries,
        "blockedReason": preview.blocked_reason,
        "operation": match kind {
            RunKind::Undo => format!("撤销记录 #{} 及之后", preview.target_id),
            RunKind::Redo => format!("重做到记录 #{}", preview.target_id),
        },
        "consequences": [
            "会一次处理多条记录",
            "开始后可以中途取消，已完成的部分会保留",
        ],
    })
}

fn run_history(
    state: &AppState,
    input: &HistoryRunInput,
    kind: RunKind,
    approved: bool,
) -> AppResult<ToolOutcome> {
    let mode = state.settings.snapshot().mcp.mode;
    if mode == McpMode::ReadOnly {
        return Err(AppError::message(
            "当前是只读模式，不能撤销或重做操作记录。请在设置 → MCP 中切换到询问或完全档。",
        ));
    }
    let _exclusive = state.lock_mcp_writes();
    let plan = preview(state, kind, input.target_id)?;
    // 同一份清单在 dryRun、拒绝与确认单里复用；每次都是刚查出来的，不需要重算。
    let plan_value = preview_value(&plan, kind);
    if input.dry_run {
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "preview": plan_value,
        })));
    }
    if let Some(reason) = &plan.blocked_reason {
        // 原样透传领域层的阻断原因；不自动换目标，也不重试。
        return Err(AppError::message(reason.clone()));
    }
    let confirmed = matches!(
        (input.confirmed_steps, input.confirmed_affected),
        (Some(steps), Some(affected)) if steps == plan.steps && affected == plan.affected
    );
    if !confirmed {
        return Ok(ToolOutcome::value(json!({
            "status": "rejected",
            "reason": "confirmedSteps 或 confirmedAffected 与当前回退清单不一致，未做任何修改",
            "latest": plan_value,
        })));
    }
    if state.tasks.any_busy() {
        let active = state.tasks.active();
        return Err(conflict_error(&active));
    }

    if mode == McpMode::Ask && !approved {
        let arguments = serde_json::to_value(input)?;
        let approval = state.mcp_approvals().enqueue(
            format!(
                "AI · {} {} 条操作记录（影响 {}）",
                kind.verb(),
                plan.steps,
                plan.affected
            ),
            ApprovalAction {
                tool: kind.tool().to_owned(),
                arguments,
            },
            plan_value.clone(),
        );
        return Ok(ToolOutcome::value(json!({
            "status": "pendingApproval",
            "approvalId": approval.id,
            "preview": plan_value,
        })));
    }

    // 批准后可能已经过去几分钟；执行前再核对一次清单与数量。
    let latest = preview(state, kind, input.target_id)?;
    if latest.blocked_reason.is_some()
        || latest.steps != input.confirmed_steps.unwrap_or(0)
        || latest.affected != input.confirmed_affected.unwrap_or(0)
    {
        return Ok(ToolOutcome::value(json!({
            "status": "rejected",
            "reason": "执行前回退清单已变化，未做任何修改",
            "latest": preview_value(&latest, kind),
        })));
    }
    let started = match kind {
        RunKind::Undo => state.mcp_undo_to(input.target_id)?,
        RunKind::Redo => state.mcp_redo_to(input.target_id)?,
    };
    match started {
        TaskStartResult::Started { task } => Ok(ToolOutcome::task(
            task.task_id.clone(),
            json!({
                "status": "started",
                "taskId": task.task_id,
                "steps": latest.steps,
                "affected": latest.affected,
            }),
        )),
        TaskStartResult::Conflict { active } => Err(conflict_error(&active)),
    }
}

fn conflict_error(active: &[TaskSummary]) -> AppError {
    let label = active
        .first()
        .map(|task| task.label.as_str())
        .unwrap_or("其他任务");
    AppError::message(format!("「{label}」正在进行，请等待完成或取消"))
}

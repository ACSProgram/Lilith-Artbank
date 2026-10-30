//! MCP 网关：设置、上下文、写锁、审批队列与 MCP 发起的任务。

use std::sync::Arc;

use serde_json::Value;
use tauri::Emitter;

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::{ConflictPolicy, LaneGuard, TaskHandle, TaskKind, TaskPhase, TaskStartResult},
    },
    library::{history, metadata, query},
    mcp::{
        approval::{ApprovalQueue, ApprovalView},
        context::ReadContext,
    },
};

use super::{AppState, LIBRARY_CHANGED_EVENT, MAX_MCP_TASKS, move_resource_request};

impl AppState {
    /// 设置保存后按新的 MCP 配置重启服务；关闭时删除运行时文件。
    pub fn apply_mcp_settings(&self, settings: &crate::app::settings::McpSettings) {
        // 关闭服务或切到只读后，旧确认单已经不能执行，直接丢弃，避免留下点不动的卡片。
        if !settings.enabled || settings.mode == crate::app::settings::McpMode::ReadOnly {
            self.mcp_approvals.clear();
        }
        self.mcp.apply(self.app.clone(), settings);
    }

    pub fn mcp_status(&self) -> crate::mcp::runtime::McpStatus {
        self.mcp.status(&self.settings.snapshot().mcp)
    }

    /// 只读工具的一次快照：图库句柄、计数、设置与拼图映射缓存。
    pub(crate) fn mcp_context(&self) -> AppResult<ReadContext> {
        let info = self
            .status()?
            .ok_or_else(|| AppError::message("尚未打开图库，请先在 Lilith Artbank 中打开图库"))?;
        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库，请先在 Lilith Artbank 中打开图库"))?;
        Ok(ReadContext {
            database,
            thumbs,
            root,
            info,
            settings: self.settings.snapshot(),
            sheets: self.mcp_sheets.clone(),
            resources: self.resources(),
            images: self.images(),
            memory: self.memory(),
        })
    }

    /// MCP 写入在本进程内串行；与任务总线无关，只保护多个客户端同时调用。
    pub(crate) fn lock_mcp_writes(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.mcp_write.lock()
    }

    /// MCP 同步写入后的统一收尾：失效页面缓存与相似结果，裁剪记录，再通知前端刷新。
    pub(crate) fn after_mcp_write(&self, payload: Value, invalidate_similarity: bool) {
        query::clear_page_cache();
        if invalidate_similarity {
            self.ai.invalidate_similarity();
        }
        self.after_history_write();
        let _ = self.app.emit(LIBRARY_CHANGED_EVENT, payload);
    }

    pub(crate) fn mcp_approvals(&self) -> Arc<ApprovalQueue> {
        self.mcp_approvals.clone()
    }

    /// 待确认与执行中的列表；执行中的记录先按任务快照回写进度与终态。
    pub fn mcp_approval_views(&self) -> Vec<ApprovalView> {
        self.mcp_approvals.sync_tasks(&self.tasks.list());
        self.mcp_approvals.list()
    }

    pub fn reject_mcp_approval(&self, id: &str) -> AppResult<()> {
        self.mcp_approvals.reject(id)
    }

    /// 用户批准后执行；写操作重新解析 selector 并核对数量，变化时只返回错误，不落库。
    pub fn approve_mcp_approval(&self, id: &str) -> AppResult<Value> {
        let record = self.mcp_approvals.begin(id)?;
        let settings = self.settings.snapshot().mcp;
        if !settings.enabled {
            let error = AppError::message("MCP 服务已关闭，不能执行待确认操作");
            self.mcp_approvals.fail(id, error.to_string());
            return Err(error);
        }
        if settings.mode == crate::app::settings::McpMode::ReadOnly {
            let error = AppError::message("当前是只读模式，MCP 写入已关闭");
            self.mcp_approvals.fail(id, error.to_string());
            return Err(error);
        }
        let tool = record.action.tool.clone();
        match crate::mcp::tools::execute_approval(self, &record.action) {
            Ok(outcome) => {
                let operation = outcome
                    .value
                    .get("operationId")
                    .and_then(Value::as_i64)
                    .map_or_else(|| "-".to_owned(), |id| id.to_string());
                crate::mcp::audit::log_line(
                    "approval",
                    &format!(
                        "approval={id} tool={tool} status=approved operation={operation} task={}",
                        outcome.task_id.as_deref().unwrap_or("-")
                    ),
                );
                if let Some(task_id) = &outcome.task_id {
                    self.remember_mcp_task(task_id);
                    self.mcp_approvals
                        .attach_task(id, task_id, outcome.value.clone());
                } else if outcome.value["status"] == "rejected" {
                    // 执行前复核失败（数量或状态变化）不是成功，也不能让确认单看起来已完成。
                    let reason = outcome.value["reason"]
                        .as_str()
                        .unwrap_or("执行前核对未通过");
                    self.mcp_approvals
                        .fail_with(id, reason, Some(outcome.value.clone()));
                } else {
                    self.mcp_approvals.complete(id, outcome.value.clone());
                }
                Ok(outcome.value)
            }
            Err(error) => {
                crate::mcp::audit::log_line(
                    "approval",
                    &format!("approval={id} tool={tool} status=error error={error}"),
                );
                self.mcp_approvals.fail(id, error.to_string());
                Err(error)
            }
        }
    }

    pub(crate) fn remember_mcp_task(&self, task_id: &str) {
        let mut tasks = self.mcp_tasks.lock();
        if tasks.iter().any(|id| id == task_id) {
            return;
        }
        tasks.push_back(task_id.to_owned());
        while tasks.len() > MAX_MCP_TASKS {
            tasks.pop_front();
        }
    }

    pub(crate) fn is_mcp_task(&self, task_id: &str) -> bool {
        self.mcp_tasks.lock().iter().any(|id| id == task_id)
    }

    /// MCP 发起的移动：后台任务，完成后任务快照带 `operationId`。
    pub fn start_mcp_move_task(
        &self,
        image_ids: Vec<i64>,
        folder_path: String,
    ) -> AppResult<TaskStartResult> {
        let (database, _, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let scope = format!("{} 张", image_ids.len());
        let request = move_resource_request(&database, &image_ids, &folder_path)?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::FolderMove,
            scope,
        );
        let task_id = handle.id().to_owned();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let launched = self.tasks.launch(
            self.app.clone(),
            handle,
            request,
            ConflictPolicy::Queue,
            move |guard| {
                let database = database.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker("mcp-move".to_owned(), task.clone(), move || {
                    let _guard: LaneGuard = guard;
                    task.set_phase(TaskPhase::Running);
                    let result = metadata::move_images_with_id(
                        &database,
                        &root,
                        &image_ids,
                        &folder_path,
                        &*task,
                    );
                    match &result {
                        Ok((report, operation_id)) => {
                            if let Some(operation_id) = operation_id {
                                let _ = history::prefix_label(&database, *operation_id, "AI · ");
                            }
                            task.set_operation_id(*operation_id);
                            // 计数字段与 message 用同一份报告，别出现「跳过 1 张」但 skipped=0。
                            task.set_skipped(report.skipped as usize);
                            task.set_failed(report.failures.len());
                            for failure in &report.failures {
                                task.add_failure(failure.image_id, &failure.path, &failure.reason);
                            }
                            let mut summary =
                                format!("移动 {} 张，跳过 {} 张", report.moved, report.skipped);
                            if !report.failures.is_empty() {
                                summary.push_str(&format!("，失败 {} 张", report.failures.len()));
                            }
                            task.set_message(summary);
                        }
                        Err(error) => task.set_message(error.to_string()),
                    }
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    task.finish_from(&result.map(|_| ()));
                });
            },
        )?;
        if matches!(launched, TaskStartResult::Started { .. }) {
            self.remember_mcp_task(&task_id);
        }
        Ok(launched)
    }

    pub fn mcp_undo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let result = self.undo_to(target_id)?;
        if let TaskStartResult::Started { task } = &result {
            self.remember_mcp_task(&task.task_id);
        }
        Ok(result)
    }

    pub fn mcp_redo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let result = self.redo_to(target_id)?;
        if let TaskStartResult::Started { task } = &result {
            self.remember_mcp_task(&task.task_id);
        }
        Ok(result)
    }
}

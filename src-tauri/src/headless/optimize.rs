//! 图片空间优化：估算与执行。
//!
//! 与 GUI 共用 `start_batch_task`；执行前核对估算凭证、候选指纹与逐张 stat 都在领域层，
//! 这里只把 CLI 请求转成 `BatchTaskRequest` 并等到终态。

use std::{cell::Cell, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::Manager;

use crate::{
    app::{
        error::AppError,
        state::AppState,
        tasks::{BatchTaskKind, BatchTaskRequest, ConflictPolicy, TaskPhase, TaskScope},
        trace,
    },
    imageops::optimize::OptimizeOptions,
};

use super::{Options, wait_task_with_timeout};

/// 估算要抽样编码，执行要逐张重编码；给足 5 分钟后仍没结束就报错而不是挂死。
const OPTIMIZE_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) fn dispatch(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let state = state.inner();
    match options.action.as_deref() {
        Some("estimate") => {
            let request: OptimizeRequest = options.request()?;
            let task = super::maintenance::run_batch_with_timeout(
                app,
                request.into_batch(BatchTaskKind::OptimizeEstimate),
                cancelled,
                OPTIMIZE_TIMEOUT,
            )?;
            // 估算结果按任务 id 缓存在状态里；任务刚结束就能取。
            let estimate = state.optimize_estimate(&task.task_id);
            Ok(json!({ "task": task, "estimate": estimate }))
        }
        action => Err(match action {
            Some(action) => format!("未知命令：optimize {action}"),
            None => "未知命令：optimize".to_owned(),
        }),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct OptimizeRequest {
    scope: TaskScope,
    options: Option<OptimizeOptions>,
    sample_size: Option<usize>,
    conflict: ConflictPolicy,
}

impl Default for OptimizeRequest {
    fn default() -> Self {
        Self {
            scope: TaskScope::Library,
            options: None,
            sample_size: None,
            conflict: ConflictPolicy::Queue,
        }
    }
}

impl OptimizeRequest {
    fn into_batch(self, kind: BatchTaskKind) -> BatchTaskRequest {
        BatchTaskRequest {
            kind,
            scope: self.scope,
            optimize: self.options,
            estimate_sample_size: self.sample_size,
            conflict: self.conflict,
            ..BatchTaskRequest::default()
        }
    }
}

/// 估算与执行必须在同一进程内（估算凭证只缓存在内存里），所以用场景串起来；
/// 场景只负责估算、按请求改几张文件/行、再执行，判断留给 `tests/`。
pub(super) fn scenario(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let request: FlowRequest = options.request()?;
    let state = app.state::<AppState>();
    let state = state.inner();
    let scope = request.scope.clone();
    let options_value = request.options.clone();
    let estimate_task = super::maintenance::run_batch_with_timeout(
        app,
        BatchTaskRequest {
            kind: BatchTaskKind::OptimizeEstimate,
            scope: scope.clone(),
            optimize: options_value.clone(),
            estimate_sample_size: request.sample_size,
            conflict: ConflictPolicy::Fail,
            ..BatchTaskRequest::default()
        },
        cancelled,
        OPTIMIZE_TIMEOUT,
    )?;
    let estimate = state
        .optimize_estimate(&estimate_task.task_id)
        .ok_or_else(|| "估算任务结束但没有结果".to_owned())?;

    let mut mutation = json!({ "truncated": 0, "removed": 0, "trashed": 0 });
    if let Some(mutate) = request.mutate {
        let mut truncated = 0_u64;
        for image_id in &mutate.truncate_image_ids {
            let detail = AppError::command(state.image_detail(*image_id))
                .map_err(|error| error.to_string())?;
            std::fs::write(&detail.file_path, vec![0_u8; 64])
                .map_err(|error| format!("无法截断 {}：{error}", detail.file_path))?;
            truncated += 1;
        }
        let mut removed = 0_u64;
        for image_id in &mutate.remove_file_ids {
            let detail = AppError::command(state.image_detail(*image_id))
                .map_err(|error| error.to_string())?;
            std::fs::remove_file(&detail.file_path)
                .map_err(|error| format!("无法删除 {}：{error}", detail.file_path))?;
            removed += 1;
        }
        let trashed = if mutate.trash_image_ids.is_empty() {
            0
        } else {
            AppError::command(state.trash_images(&mutate.trash_image_ids))
                .map_err(|error| error.to_string())?
        };
        mutation = json!({
            "truncated": truncated,
            "removed": removed,
            "trashed": trashed,
        });
    }

    let run_options = request.run_options.or(options_value);
    let start = trace::write("start_batch_task", || {
        AppError::command(state.start_batch_task(
            app.clone(),
            BatchTaskRequest {
                kind: BatchTaskKind::Optimize,
                scope,
                optimize: run_options,
                estimate_id: Some(estimate.task_id.clone()),
                conflict: ConflictPolicy::Fail,
                ..BatchTaskRequest::default()
            },
        ))
    })?;
    let started = super::maintenance::started_task(start)?;
    let snapshot = wait_task_with_timeout(state, &started.task_id, OPTIMIZE_TIMEOUT)?;
    let error = match snapshot.phase {
        TaskPhase::Failed => Some(snapshot.message.clone()),
        _ => None,
    };
    let history = AppError::command(state.history_page())?;
    Ok(json!({
        "estimate": estimate,
        "mutation": mutation,
        "run": snapshot,
        "error": error,
        "history": history,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlowRequest {
    #[serde(default)]
    scope: TaskScope,
    #[serde(default)]
    options: Option<OptimizeOptions>,
    #[serde(default)]
    sample_size: Option<usize>,
    #[serde(default)]
    run_options: Option<OptimizeOptions>,
    #[serde(default)]
    mutate: Option<MutationRequest>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct MutationRequest {
    truncate_image_ids: Vec<i64>,
    remove_file_ids: Vec<i64>,
    trash_image_ids: Vec<i64>,
}

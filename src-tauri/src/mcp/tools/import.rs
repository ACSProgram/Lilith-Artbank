//! MCP 导入工具。
//!
//! 导入固定复用界面的扫描与入库管线：不传 `optimize`，也不接受 `workerThreads`。
//! 询问档每次都要用户确认；只读档直接拒绝。导入是后台任务，完成后 `tasks_get`
//! 会附带导入回执，整次导入可以用 `history_undo` 撤销。

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::McpMode,
        state::AppState,
    },
    import::{FolderMode, ImportRequest, job::DuplicatePolicy},
};

use super::{ToolOutcome, write};

const MAX_PATHS_REPORTED: usize = 20;
const DEFAULT_PERCEPTUAL_THRESHOLD: u8 = 6;

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct ImportStartInput {
    #[schemars(description = "本机绝对路径，文件或目录；单次最多 10000 个输入路径")]
    pub paths: Vec<String>,
    #[schemars(description = "图库内目标文件夹相对路径；省略或空字符串表示根目录")]
    pub folder: Option<String>,
    #[schemars(
        description = "flat=全部放进目标文件夹；preserve=保留每个输入目录下的相对层级；默认 flat"
    )]
    pub folder_mode: Option<String>,
    #[schemars(description = "导入时附加的标签，最多 32 个")]
    pub tags: Vec<String>,
    #[schemars(description = "导入时的初始评分 0-5；默认 0")]
    pub score: Option<u8>,
    #[schemars(description = "skip=重复内容不入库；merge=把标签/评分合并到已有图片；默认 skip")]
    pub duplicate_policy: Option<String>,
}

pub fn import_start(state: &AppState, input: &ImportStartInput) -> AppResult<ToolOutcome> {
    run_import(state, input, false)
}

pub fn execute_import(state: &AppState, input: &ImportStartInput) -> AppResult<ToolOutcome> {
    run_import(state, input, true)
}

fn run_import(
    state: &AppState,
    input: &ImportStartInput,
    approved: bool,
) -> AppResult<ToolOutcome> {
    let mode = write::ensure_writable(state)?;
    let request = prepare_request(input)?;
    let target = target_label(&request);
    let _exclusive = state.lock_mcp_writes();

    if mode == McpMode::Ask && !approved {
        return write::enqueue_approval(
            state,
            "import_start",
            format!("AI · 导入 {} 个路径到{target}", request.paths.len()),
            input,
            preview(&request, &target),
        );
    }

    let paths = request.paths.clone();
    let (snapshot, operation_id) = state.start_mcp_import(request)?;
    let task_id = snapshot.progress.job_id.clone();
    crate::mcp::audit::import_sources(&paths, &target, mode.as_str(), &task_id, operation_id);
    Ok(ToolOutcome::task(
        task_id.clone(),
        json!({
            "status": "started",
            "taskId": task_id,
            "pathCount": paths.len(),
            "target": target,
            "operationId": operation_id,
            "compressed": false,
            "message": "导入在后台进行；用 tasks_get 轮询，完成后可 history_undo 撤销整次导入。",
        }),
    ))
}

/// 组装并规范化导入请求；`optimize` 永远为 `None`，`workerThreads` 由设置决定。
fn prepare_request(input: &ImportStartInput) -> AppResult<ImportRequest> {
    let request = ImportRequest {
        paths: input.paths.clone(),
        folder: input.folder.clone().unwrap_or_default(),
        folder_mode: folder_mode(input.folder_mode.as_deref())?,
        tags: input.tags.clone(),
        score: input.score.unwrap_or(0),
        duplicate_policy: duplicate_policy(input.duplicate_policy.as_deref())?,
        perceptual_threshold: DEFAULT_PERCEPTUAL_THRESHOLD,
        optimize: None,
        worker_threads: None,
        #[cfg(test)]
        cancel_after: None,
    };
    request.normalized()
}

fn folder_mode(value: Option<&str>) -> AppResult<FolderMode> {
    match value.unwrap_or("flat").trim().to_ascii_lowercase().as_str() {
        "flat" => Ok(FolderMode::Flat),
        "preserve" => Ok(FolderMode::Preserve),
        other => Err(AppError::message(format!(
            "folderMode 只能是 flat 或 preserve，收到 {other}"
        ))),
    }
}

fn duplicate_policy(value: Option<&str>) -> AppResult<DuplicatePolicy> {
    match value.unwrap_or("skip").trim().to_ascii_lowercase().as_str() {
        "skip" => Ok(DuplicatePolicy::Skip),
        "merge" => Ok(DuplicatePolicy::Merge),
        other => Err(AppError::message(format!(
            "duplicatePolicy 只能是 skip 或 merge，收到 {other}"
        ))),
    }
}

fn target_label(request: &ImportRequest) -> String {
    let folder = if request.folder.is_empty() {
        "根目录".to_owned()
    } else {
        format!("「{}」", request.folder)
    };
    match request.folder_mode {
        FolderMode::Flat => folder,
        FolderMode::Preserve => format!("{folder}（保留源目录）"),
    }
}

fn preview(request: &ImportRequest, target: &str) -> Value {
    let paths = request
        .paths
        .iter()
        .take(MAX_PATHS_REPORTED)
        .cloned()
        .collect::<Vec<_>>();
    json!({
        "operation": "导入图片",
        "target": target,
        "count": request.paths.len(),
        "unit": "path",
        "pathTotal": request.paths.len(),
        "paths": paths,
        "tags": request.tags,
        "score": request.score,
        "duplicatePolicy": match request.duplicate_policy {
            DuplicatePolicy::Skip => "skip",
            DuplicatePolicy::Merge => "merge",
        },
        "consequences": [
            "图片会复制进图库，原文件不移动",
            "不会压缩图片",
            "完成后可以撤销整次导入",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(paths: Vec<String>) -> ImportStartInput {
        ImportStartInput {
            paths,
            ..Default::default()
        }
    }

    fn absolute_temp_path() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    #[test]
    fn builds_a_request_without_optimization() {
        let request = prepare_request(&input(vec![absolute_temp_path()])).unwrap();
        assert!(request.optimize.is_none());
        assert!(request.worker_threads.is_none());
        assert_eq!(request.folder_mode, FolderMode::Flat);
        assert_eq!(request.duplicate_policy, DuplicatePolicy::Skip);
    }

    #[test]
    fn ignores_unknown_optimize_and_worker_fields() {
        let value = json!({
            "paths": [absolute_temp_path()],
            "optimize": { "tier": "high", "outputFormat": "jpeg" },
            "workerThreads": 16,
        });
        let parsed: ImportStartInput = serde_json::from_value(value).unwrap();
        let request = prepare_request(&parsed).unwrap();
        assert!(request.optimize.is_none());
        assert!(request.worker_threads.is_none());
    }

    #[test]
    fn rejects_relative_paths_and_unknown_modes() {
        assert!(prepare_request(&input(vec!["relative.png".into()])).is_err());

        let mut value = input(vec![absolute_temp_path()]);
        value.folder_mode = Some("nested".into());
        assert!(prepare_request(&value).is_err());

        let mut value = input(vec![absolute_temp_path()]);
        value.duplicate_policy = Some("overwrite".into());
        assert!(prepare_request(&value).is_err());
    }

    #[test]
    fn preview_lists_at_most_twenty_paths() {
        let paths = (0..25)
            .map(|index| format!("{}\\{index}.png", absolute_temp_path()))
            .collect::<Vec<_>>();
        let request = prepare_request(&input(paths)).unwrap();
        let value = preview(&request, "根目录");
        assert_eq!(value["count"], 25);
        assert_eq!(value["unit"], "path");
        assert_eq!(value["paths"].as_array().unwrap().len(), 20);
        assert_eq!(value["pathTotal"], 25);
    }
}

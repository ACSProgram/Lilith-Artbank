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
    import::{FolderMode, ImportRequest, ImportTarget, job::DuplicatePolicy},
};

use super::{ToolOutcome, write};

const MAX_PATHS_REPORTED: usize = 20;
const DEFAULT_PERCEPTUAL_THRESHOLD: u8 = 6;

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct ImportStartInput {
    #[schemars(description = "本机绝对路径，文件或目录；单次最多 10000 个输入路径")]
    pub paths: Vec<String>,
    #[schemars(
        description = "library=普通图库（默认）；temp=临时文件夹，只能平铺、只能跳过重复，不写操作记录"
    )]
    pub target: Option<String>,
    #[schemars(
        description = "图库内目标文件夹相对路径；省略或空字符串表示根目录；target=temp 时忽略"
    )]
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
    let temporary = request.target == ImportTarget::Temp;
    let target = target_label(&request);
    let _exclusive = state.lock_mcp_writes();

    // 路径不存在就别排队等用户批准了：批准之后也只会失败。
    if let Some(outcome) = missing_paths(&request.paths) {
        return Ok(outcome);
    }

    // 临时导入是查找用的暂存区，询问档下免确认；普通导入仍然每次确认。
    if mode == McpMode::Ask && !approved && !temporary {
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
    let message = if temporary {
        "临时导入不写操作记录；关闭、切换图库或退出应用时会清空。用 tasks_get 轮询。"
    } else {
        "导入在后台进行；用 tasks_get 轮询，完成后可 history_undo 撤销整次导入。"
    };
    Ok(ToolOutcome::task(
        task_id.clone(),
        json!({
            "status": "started",
            "taskId": task_id,
            "pathCount": paths.len(),
            "target": target,
            "temporary": temporary,
            "operationId": operation_id,
            "compressed": false,
            "message": message,
        }),
    ))
}

/// 入队前确认每个输入路径都存在；缺失的直接拒绝并列出。
fn missing_paths(paths: &[String]) -> Option<ToolOutcome> {
    let missing: Vec<String> = paths
        .iter()
        .filter(|path| std::fs::metadata(path).is_err())
        .take(MAX_PATHS_REPORTED)
        .cloned()
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(ToolOutcome::value(json!({
        "status": "rejected",
        "reason": "有路径不存在或无法访问，未开始导入",
        "missingPaths": missing,
        "pathTotal": paths.len(),
    })))
}

/// 组装并规范化导入请求；`optimize` 永远为 `None`，`workerThreads` 由设置决定。
fn prepare_request(input: &ImportStartInput) -> AppResult<ImportRequest> {
    let request = ImportRequest {
        paths: input.paths.clone(),
        target: target(input.target.as_deref())?,
        folder: input.folder.clone().unwrap_or_default(),
        folder_mode: folder_mode(input.folder_mode.as_deref())?,
        tags: input.tags.clone(),
        score: input.score.unwrap_or(0),
        duplicate_policy: duplicate_policy(input.duplicate_policy.as_deref())?,
        perceptual_threshold: DEFAULT_PERCEPTUAL_THRESHOLD,
        optimize: None,
        #[cfg(test)]
        cancel_after: None,
    };
    request.normalized()
}

fn target(value: Option<&str>) -> AppResult<ImportTarget> {
    match value
        .unwrap_or("library")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "library" => Ok(ImportTarget::Library),
        "temp" => Ok(ImportTarget::Temp),
        other => Err(AppError::message(format!(
            "target 只能是 library 或 temp，收到 {other}"
        ))),
    }
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
    if request.target == ImportTarget::Temp {
        return "临时文件夹".to_owned();
    }
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
    let temporary = request.target == ImportTarget::Temp;
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
        "consequences": if temporary {
            vec![
                "图片会复制到临时文件夹，原文件不移动",
                "不会压缩图片",
                "不写操作记录；关闭、切换图库或退出应用时清空",
            ]
        } else {
            vec![
                "图片会复制进图库，原文件不移动",
                "不会压缩图片",
                "完成后可以撤销整次导入",
            ]
        },
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
        assert_eq!(request.folder_mode, FolderMode::Flat);
        assert_eq!(request.duplicate_policy, DuplicatePolicy::Skip);
    }

    #[test]
    fn temp_target_clears_folder_and_rejects_merge_or_preserve() {
        let mut value = input(vec![absolute_temp_path()]);
        value.target = Some("temp".into());
        value.folder = Some("人物".into());
        let request = prepare_request(&value).unwrap();
        assert_eq!(request.target, ImportTarget::Temp);
        assert_eq!(request.folder, "");

        value.duplicate_policy = Some("merge".into());
        assert!(prepare_request(&value).is_err());

        value.duplicate_policy = Some("skip".into());
        value.folder_mode = Some("preserve".into());
        assert!(prepare_request(&value).is_err());
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
    fn missing_paths_are_rejected_before_queueing() {
        let existing = absolute_temp_path();
        assert!(missing_paths(std::slice::from_ref(&existing)).is_none());

        let missing = format!("{existing}/definitely-not-here.png");
        let outcome = missing_paths(&[existing, missing]).unwrap().value;
        assert_eq!(outcome["status"], "rejected");
        assert_eq!(outcome["missingPaths"].as_array().unwrap().len(), 1);
        assert_eq!(outcome["pathTotal"], 2);
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

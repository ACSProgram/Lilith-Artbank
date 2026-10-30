//! MCP 标签工具：重命名、删除、合并。
//!
//! 三种动作都复用 `library::metadata` 的领域实现，因此都走操作记录、都能撤销。
//! 询问档对删除与合并要用户确认（会成批移除关系），重命名直接执行——与
//! `folders_modify` 的划分一致。标签操作没有 selector，所以不套 100 张阈值，
//! 改用 `expectedImages` 核对“源标签当前有多少张图片”。

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::McpMode,
        state::AppState,
    },
    library::{db::LibraryDb, metadata},
};

use super::{
    ToolOutcome,
    write::{self, WriteFinish},
};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TagAction {
    Rename,
    Delete,
    Merge,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TagsModifyInput {
    pub action: TagAction,
    #[schemars(description = "要重命名、删除或合并掉的标签 id")]
    pub tag_id: Option<i64>,
    #[schemars(description = "action=rename 时的新名称，最多 64 个字符")]
    #[serde(default)]
    pub name: Option<String>,
    #[schemars(description = "action=merge 时要保留的目标标签 id")]
    #[serde(default)]
    pub target_id: Option<i64>,
    #[schemars(description = "删除或合并前核对源标签当前图片数；不一致就拒绝")]
    #[serde(default)]
    pub expected_images: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

pub fn tags_modify(state: &AppState, input: &TagsModifyInput) -> AppResult<ToolOutcome> {
    run_tags_modify(state, input, false)
}

pub fn execute_tags_modify(state: &AppState, input: &TagsModifyInput) -> AppResult<ToolOutcome> {
    run_tags_modify(state, input, true)
}

/// 询问档只拦会成批移除关系的动作；重命名可以撤销，直接执行。
fn needs_approval(mode: McpMode, action: TagAction) -> bool {
    matches!(action, TagAction::Delete | TagAction::Merge) && mode == McpMode::Ask
}

fn run_tags_modify(
    state: &AppState,
    input: &TagsModifyInput,
    approved: bool,
) -> AppResult<ToolOutcome> {
    let gate = write::WriteGate::enter(state)?;
    let mode = gate.mode;
    let database = &gate.database;
    let tag_id = input
        .tag_id
        .ok_or_else(|| AppError::message("tagId 不能为空"))?;

    match input.action {
        TagAction::Rename => rename_tag(state, database, input, tag_id),
        TagAction::Delete => {
            let name = metadata::tag_name(database, tag_id)?
                .ok_or_else(|| AppError::message("标签不存在"))?;
            let images = metadata::tag_image_count(database, tag_id)?;
            if let Some(outcome) = expected_mismatch(input.expected_images, images) {
                return Ok(outcome);
            }
            let plan = delete_preview(&name, tag_id, images);
            if input.dry_run {
                return Ok(ToolOutcome::value(plan));
            }
            if needs_approval(mode, input.action) && !approved {
                return write::enqueue_approval(
                    state,
                    "tags_modify",
                    format!("AI · 删除标签「{name}」"),
                    input,
                    plan,
                );
            }
            let lane = state.begin_metadata_write("修改图片信息")?;
            let (affected, operation_id) = metadata::delete_tag_with_id(database, tag_id)?;
            drop(lane);
            Ok(ToolOutcome::value(write::finish_write(
                state,
                WriteFinish {
                    tool: "tags_modify",
                    operation_id,
                    selected: images as usize,
                    affected,
                    close_viewer: false,
                    invalidate_similarity: false,
                    change: json!({ "kind": "tagCatalog", "tagAction": "delete", "tagId": tag_id }),
                    // 标签行本身已经删掉，0 张图片的标签也不是“没有变化”。
                    extra: json!({ "status": "ok", "images": images }),
                },
            )))
        }
        TagAction::Merge => {
            let target_id = input
                .target_id
                .ok_or_else(|| AppError::message("action=merge 时必须给 targetId"))?;
            let preview = metadata::merge_preview(database, tag_id, target_id)?;
            if let Some(outcome) = expected_mismatch(input.expected_images, preview.source_images) {
                return Ok(outcome);
            }
            let plan = merge_preview_value(&preview, tag_id, target_id);
            if input.dry_run {
                return Ok(ToolOutcome::value(plan));
            }
            if needs_approval(mode, input.action) && !approved {
                return write::enqueue_approval(
                    state,
                    "tags_modify",
                    format!(
                        "AI · 合并标签「{}」→「{}」",
                        preview.source_name, preview.target_name
                    ),
                    input,
                    plan,
                );
            }
            let lane = state.begin_metadata_write("修改图片信息")?;
            let (report, operation_id) = metadata::merge_tags_with_id(database, tag_id, target_id)?;
            drop(lane);
            Ok(ToolOutcome::value(write::finish_write(
                state,
                WriteFinish {
                    tool: "tags_modify",
                    operation_id,
                    selected: report.source_images as usize,
                    affected: report.source_images,
                    close_viewer: false,
                    invalidate_similarity: false,
                    change: json!({
                        "kind": "tagCatalog",
                        "tagAction": "merge",
                        "tagId": tag_id,
                        "targetTagId": target_id,
                    }),
                    extra: json!({
                        // 源标签行已删除，0 张图片的标签也不是“没有变化”。
                        "status": "ok",
                        "moved": report.moved,
                        "duplicate": report.duplicate,
                        "target": { "id": target_id, "name": report.target_name },
                    }),
                },
            )))
        }
    }
}

fn rename_tag(
    state: &AppState,
    database: &LibraryDb,
    input: &TagsModifyInput,
    tag_id: i64,
) -> AppResult<ToolOutcome> {
    let name = input.name.as_deref().unwrap_or("").trim();
    if name.is_empty() {
        return Err(AppError::message("name 不能为空"));
    }
    let current =
        metadata::tag_name(database, tag_id)?.ok_or_else(|| AppError::message("标签不存在"))?;
    if current == name {
        return Ok(ToolOutcome::value(json!({
            "status": "noop",
            "affected": 0,
            "reason": "标签已经是这个名称",
            "tag": { "id": tag_id, "name": current },
        })));
    }
    if input.dry_run {
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "operation": format!("重命名标签「{current}」→「{name}」"),
            "tag": { "id": tag_id, "name": current },
            "willChange": 1,
            "consequences": ["只改标签名称，图片关系不变；可以撤销"],
        })));
    }
    let lane = state.begin_metadata_write("修改图片信息")?;
    let operation_id = metadata::rename_tag_with_id(database, tag_id, name)?;
    drop(lane);
    Ok(ToolOutcome::value(write::finish_write(
        state,
        WriteFinish {
            tool: "tags_modify",
            operation_id,
            selected: 1,
            affected: 1,
            close_viewer: false,
            invalidate_similarity: false,
            change: json!({ "kind": "tagCatalog", "tagAction": "rename", "tagId": tag_id }),
            extra: json!({
                "tag": { "id": tag_id, "name": name },
                "previousName": current,
            }),
        },
    )))
}

fn delete_preview(name: &str, tag_id: i64, images: u64) -> Value {
    json!({
        "status": "dryRun",
        "operation": format!("删除标签「{name}」"),
        "tag": { "id": tag_id, "name": name },
        "count": images,
        "willChange": images,
        "consequences": [
            format!("会从 {images} 张图片上移除这个标签"),
            "图片本身不会删除；可以在操作记录里撤销",
        ],
    })
}

fn merge_preview_value(report: &metadata::MergeReport, source_id: i64, target_id: i64) -> Value {
    json!({
        "status": "dryRun",
        "operation": format!(
            "合并标签「{}」→「{}」",
            report.source_name, report.target_name
        ),
        "source": { "id": source_id, "name": report.source_name, "images": report.source_images },
        "target": { "id": target_id, "name": report.target_name },
        "count": report.source_images,
        "willChange": report.moved,
        "duplicate": report.duplicate,
        "consequences": [
            format!("{} 张图片会改挂到「{}」", report.moved, report.target_name),
            format!("其中 {} 张本来就有目标标签，只去掉重复", report.duplicate),
            format!("源标签「{}」会被删除；可以在操作记录里撤销", report.source_name),
        ],
    })
}

fn expected_mismatch(expected: Option<u64>, actual: u64) -> Option<ToolOutcome> {
    let expected = expected?;
    (expected != actual).then(|| {
        ToolOutcome::value(json!({
            "status": "rejected",
            "reason": format!("标签当前有 {actual} 张图片，与 expectedImages={expected} 不一致"),
            "images": actual,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_only_covers_destructive_actions() {
        assert!(!needs_approval(McpMode::Ask, TagAction::Rename));
        assert!(needs_approval(McpMode::Ask, TagAction::Delete));
        assert!(needs_approval(McpMode::Ask, TagAction::Merge));
        assert!(!needs_approval(McpMode::Full, TagAction::Merge));
        assert!(!needs_approval(McpMode::ReadOnly, TagAction::Delete));
    }

    #[test]
    fn expected_images_must_match() {
        assert!(expected_mismatch(None, 3).is_none());
        assert!(expected_mismatch(Some(3), 3).is_none());
        assert!(expected_mismatch(Some(2), 3).is_some());
    }
}

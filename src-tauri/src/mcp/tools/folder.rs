//! MCP 文件夹工具：新建、重命名、移动、删除。
//!
//! 全部复用 `library::folders` 的领域实现，因此都走操作记录、都能撤销；临时文件夹
//! 一律不能改名、移动或删除。询问档对删除与移动要用户确认（会改动一批图片的路径），
//! 新建与重命名直接执行。

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::McpMode,
        state::AppState,
    },
    concurrency::resources::PathScope,
    library::{
        db::LibraryDb,
        folders,
        paths::{folder_within, join_folder, normalize_folder, normalize_folder_name, parent_path},
        query::FolderInfo,
    },
};

use super::{ToolOutcome, merge_object, write};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FolderAction {
    Create,
    Rename,
    Move,
    Delete,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FoldersModifyInput {
    pub action: FolderAction,
    #[schemars(description = "action=create 时要建的相对路径，例如 作品/速写；缺失的上级会一起建")]
    #[serde(default)]
    pub path: Option<String>,
    #[schemars(description = "action=rename/move/delete 时的目标文件夹 id；省略时用 path 指定")]
    #[serde(default)]
    pub folder_id: Option<i64>,
    #[schemars(description = "action=rename 时的新名称，不能带路径分隔符")]
    #[serde(default)]
    pub name: Option<String>,
    #[schemars(
        description = "action=move 时的新上级相对路径；空字符串表示根目录；与 parentId 二选一"
    )]
    #[serde(default)]
    pub parent_path: Option<String>,
    #[schemars(description = "action=move 时的新上级文件夹 id；与 parentPath 二选一")]
    #[serde(default)]
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub dry_run: bool,
}

pub fn folders_modify(state: &AppState, input: &FoldersModifyInput) -> AppResult<ToolOutcome> {
    run_folders_modify(state, input, false)
}

pub fn execute_folders_modify(
    state: &AppState,
    input: &FoldersModifyInput,
) -> AppResult<ToolOutcome> {
    run_folders_modify(state, input, true)
}

/// 询问档只拦会改动一批图片路径的操作。
fn needs_approval(mode: McpMode, action: FolderAction) -> bool {
    mode == McpMode::Ask && matches!(action, FolderAction::Move | FolderAction::Delete)
}

fn run_folders_modify(
    state: &AppState,
    input: &FoldersModifyInput,
    approved: bool,
) -> AppResult<ToolOutcome> {
    let gate = write::WriteGate::enter(state)?;
    let mode = gate.mode;
    let database = &gate.database;
    let root = &gate.root;

    match input.action {
        FolderAction::Create => {
            let requested = input.path.as_deref().unwrap_or("");
            let path = normalize_folder(requested)?;
            if path.is_empty() {
                return Err(AppError::message("path 不能为空；新建根目录没有意义"));
            }
            if folders::find_id(database, &path)?.is_some() {
                return Ok(already_exists(&path));
            }
            let plan = json!({
                "operation": format!("新建文件夹「{path}」"),
                "target": path,
                "count": 1,
                "unit": "folder",
                "folderPath": path,
                "willChange": 1,
                "consequences": [
                    "只新建空文件夹，不改动任何图片",
                    "可以撤销",
                ],
            });
            if input.dry_run {
                return Ok(dry_run_outcome(plan));
            }
            let _guard = state.begin_folder_change(
                "新建文件夹",
                vec![
                    PathScope::write(parent_path(&path)),
                    PathScope::write(&path),
                ],
            )?;
            let (list, operation_id) = folders::create_with_id(database, root, &path)?;
            Ok(ToolOutcome::value(finish(
                state,
                "folders_modify",
                operation_id,
                find_info(&list, &path),
                plan,
            )))
        }
        FolderAction::Rename => {
            let target = resolve_target(database, input)?;
            let current = folders::info(database, target)?;
            if current.path.is_empty() {
                return Err(AppError::message("不能重命名图库根目录"));
            }
            reject_temp_folder(database, &current.path)?;
            let name = normalize_folder_name(input.name.as_deref().unwrap_or(""))?;
            let new_path = join_folder(parent_path(&current.path), &name);
            if new_path == current.path {
                return Ok(no_change("文件夹名称没有变化"));
            }
            if let Some(existing) = folders::find_id(database, &new_path)?
                && existing != target
            {
                return Ok(rejected(format!("「{new_path}」已经存在")));
            }
            let plan = json!({
                "operation": format!("把「{}」重命名为「{new_path}」", current.path),
                "target": new_path,
                "count": 1,
                "unit": "folder",
                "folderId": target,
                "folderPath": new_path,
                "previousPath": current.path,
                "imageCount": current.total_image_count,
                "willChange": 1,
                "consequences": [
                    format!("{} 张图片的文件夹路径会跟着变", current.total_image_count),
                    "原图不会被移动或重新编码",
                    "可以撤销",
                ],
            });
            if input.dry_run {
                return Ok(dry_run_outcome(plan));
            }
            let _guard = state.begin_folder_change(
                "重命名文件夹",
                vec![PathScope::write(&current.path), PathScope::write(&new_path)],
            )?;
            let (list, operation_id) = folders::rename_with_id(database, root, target, &name)?;
            Ok(ToolOutcome::value(finish(
                state,
                "folders_modify",
                operation_id,
                find_info(&list, &new_path),
                plan,
            )))
        }
        FolderAction::Move => {
            let target = resolve_target(database, input)?;
            let current = folders::info(database, target)?;
            if current.path.is_empty() {
                return Err(AppError::message("不能移动图库根目录"));
            }
            reject_temp_folder(database, &current.path)?;
            let parent = resolve_parent(database, input)?;
            reject_temp_folder(database, &parent.path)?;
            if folder_within(&parent.path, &current.path) {
                return Ok(rejected("不能把文件夹移动到自身或子文件夹里"));
            }
            let leaf = current
                .path
                .rsplit_once('/')
                .map_or(current.path.as_str(), |(_, leaf)| leaf);
            let new_path = join_folder(&parent.path, leaf);
            if new_path == current.path {
                return Ok(no_change("文件夹已经在这个位置"));
            }
            if let Some(existing) = folders::find_id(database, &new_path)?
                && existing != target
            {
                return Ok(rejected(format!("目标位置已经有「{new_path}」")));
            }
            let plan = json!({
                "operation": format!("把「{}」移动到「{new_path}」", current.path),
                "target": new_path,
                "count": 1,
                "unit": "folder",
                "folderId": target,
                "folderPath": new_path,
                "previousPath": current.path,
                "imageCount": current.total_image_count,
                "willChange": 1,
                "consequences": [
                    format!("{} 张图片的文件夹路径会跟着变", current.total_image_count),
                    "原图会跟着移动到新目录",
                    "可以撤销",
                ],
            });
            if input.dry_run {
                return Ok(dry_run_outcome(plan));
            }
            if needs_approval(mode, input.action) && !approved {
                return write::enqueue_approval(
                    state,
                    "folders_modify",
                    format!("AI · 移动文件夹「{}」", current.path),
                    input,
                    plan,
                );
            }
            let _guard = state.begin_folder_change(
                "移动文件夹",
                vec![
                    PathScope::write(&current.path),
                    PathScope::write(&new_path),
                    PathScope::write(&parent.path),
                ],
            )?;
            let (list, operation_id) = folders::move_to_with_id(database, root, target, parent.id)?;
            Ok(ToolOutcome::value(finish(
                state,
                "folders_modify",
                operation_id,
                find_info(&list, &new_path),
                plan,
            )))
        }
        FolderAction::Delete => {
            let target = resolve_target(database, input)?;
            let current = folders::info(database, target)?;
            if let Some(reason) = folders::deletion_blocker(database, target)? {
                return Ok(rejected(reason));
            }
            let subfolder_count = folders::subtree_folder_count(database, target)?;
            let plan = json!({
                "operation": format!("把文件夹「{}」移入回收站", current.path),
                "target": current.path,
                "count": 1,
                "unit": "folder",
                "folderId": target,
                "folderPath": current.path,
                "imageCount": current.total_image_count,
                "subfolderCount": subfolder_count,
                "recoverable": true,
                "willChange": 1 + current.total_image_count,
                "consequences": [
                    format!(
                        "{} 张图片和 {} 个子文件夹会一起移入回收站",
                        current.total_image_count, subfolder_count
                    ),
                    "原文件留在原路径，不会立即删除",
                    "可以在回收站里恢复整个文件夹",
                ],
            });
            if input.dry_run {
                return Ok(dry_run_outcome(plan.clone()));
            }
            if needs_approval(mode, input.action) && !approved {
                return write::enqueue_approval(
                    state,
                    "folders_modify",
                    format!("AI · 把文件夹「{}」移入回收站", current.path),
                    input,
                    plan,
                );
            }
            let _guard =
                state.begin_folder_change("移入回收站", vec![PathScope::write(&current.path)])?;
            let (_, operation_id) = folders::trash_with_id(database, target)?;
            Ok(ToolOutcome::value(finish(
                state,
                "folders_modify",
                operation_id,
                Some(current),
                plan,
            )))
        }
    }
}

/// 临时文件夹不能改名、移动或删除；在预览与确认之前就挡住，别让用户白确认一次。
fn reject_temp_folder(database: &LibraryDb, path: &str) -> AppResult<()> {
    let temp_path = database.with_reader(crate::library::temp::path)?;
    if temp_path.as_deref() == Some(path) {
        return Err(AppError::message(
            "临时文件夹不能重命名、移动或删除；关闭或切换图库时会自动清空",
        ));
    }
    Ok(())
}

/// rename/move/delete 的目标：优先 `folderId`，其次 `path`。
fn resolve_target(database: &LibraryDb, input: &FoldersModifyInput) -> AppResult<i64> {
    if let Some(id) = input.folder_id {
        if id <= 0 {
            return Err(AppError::message(format!("文件夹 id 无效：{id}")));
        }
        return Ok(id);
    }
    let path = input.path.as_deref().unwrap_or("");
    if path.trim().is_empty() {
        return Err(AppError::message(
            "action=rename/move/delete 需要 folderId 或 path 指定文件夹",
        ));
    }
    folders::find_id(database, path)?
        .ok_or_else(|| AppError::message(format!("文件夹不存在：{path}")))
}

fn resolve_parent(database: &LibraryDb, input: &FoldersModifyInput) -> AppResult<FolderInfo> {
    if let Some(id) = input.parent_id {
        if id <= 0 {
            return Err(AppError::message(format!("父文件夹 id 无效：{id}")));
        }
        return folders::info(database, id);
    }
    let path = normalize_folder(input.parent_path.as_deref().unwrap_or(""))?;
    let id = folders::find_id(database, &path)?
        .ok_or_else(|| AppError::message(format!("父文件夹不存在：{path}")))?;
    folders::info(database, id)
}

fn find_info(list: &[FolderInfo], path: &str) -> Option<FolderInfo> {
    list.iter()
        .find(|folder| folder.path.eq_ignore_ascii_case(path))
        .cloned()
}

fn finish(
    state: &AppState,
    tool: &'static str,
    operation_id: Option<i64>,
    folder: Option<FolderInfo>,
    plan: Value,
) -> Value {
    // 与其它写入路径同口径：记录标题前打「AI · 」，否则操作记录里分不出是 AI 改的。
    if let Some((database, _)) = state.current_library() {
        mark_ai_origin(&database, operation_id);
    }
    if operation_id.is_some() {
        state.after_mcp_write(
            json!({
                "source": "mcp",
                "tool": tool,
                "operationId": operation_id,
                // 文件夹改名/移动会换掉网格里的路径，让界面刷新一次。
                "closeViewer": false,
                "change": { "kind": "folder" },
            }),
            false,
        );
    }
    let mut value = json!({
        "status": "ok",
        "tool": tool,
        "affected": 1,
        "operationId": operation_id,
        "folder": folder.map(|folder| json!({
            "id": folder.id,
            "path": folder.path,
            "directCount": folder.direct_image_count,
            "totalCount": folder.total_image_count,
        })),
    });
    merge_object(&mut value, plan);
    value
}

/// 给操作记录打「AI · 」前缀；其它写入路径（评分、导入等）都这么做，文件夹工具不能例外。
fn mark_ai_origin(database: &LibraryDb, operation_id: Option<i64>) {
    let Some(operation_id) = operation_id else {
        return;
    };
    let _ = crate::library::history::prefix_label(database, operation_id, "AI · ");
}

fn dry_run_outcome(plan: Value) -> ToolOutcome {
    let mut value = json!({ "status": "dryRun" });
    merge_object(&mut value, plan);
    ToolOutcome::value(value)
}

fn rejected(reason: impl Into<String>) -> ToolOutcome {
    ToolOutcome::value(json!({
        "status": "rejected",
        "reason": reason.into(),
    }))
}

fn already_exists(path: &str) -> ToolOutcome {
    ToolOutcome::value(json!({
        "status": "noop",
        "affected": 0,
        "reason": format!("文件夹「{path}」已经存在"),
    }))
}

fn no_change(reason: impl Into<String>) -> ToolOutcome {
    ToolOutcome::value(json!({
        "status": "noop",
        "affected": 0,
        "reason": reason.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::service;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, std::path::PathBuf, std::sync::Arc<LibraryDb>) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "文件夹测试", 1).unwrap();
        (temp, root, session.db)
    }

    #[test]
    fn resolves_target_and_parent_by_path() {
        let (_temp, root, database) = fixture();
        folders::create(&database, &root, "作品/速写").unwrap();

        let target = resolve_target(
            &database,
            &FoldersModifyInput {
                action: FolderAction::Rename,
                path: Some("作品/速写".into()),
                folder_id: None,
                name: None,
                parent_path: None,
                parent_id: None,
                dry_run: false,
            },
        )
        .unwrap();
        assert_eq!(folders::info(&database, target).unwrap().path, "作品/速写");

        // 空 parentPath 表示根目录：根目录行存在，可以解析出 id。
        let parent = resolve_parent(
            &database,
            &FoldersModifyInput {
                action: FolderAction::Move,
                path: None,
                folder_id: None,
                name: None,
                parent_path: Some(String::new()),
                parent_id: None,
                dry_run: false,
            },
        )
        .unwrap();
        assert_eq!(parent.path, "");

        let error = resolve_target(
            &database,
            &FoldersModifyInput {
                action: FolderAction::Delete,
                path: Some("不存在".into()),
                folder_id: None,
                name: None,
                parent_path: None,
                parent_id: None,
                dry_run: false,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("文件夹不存在"));
    }

    #[test]
    fn deletion_moves_non_empty_folders_to_trash() {
        let (_temp, root, database) = fixture();
        folders::create(&database, &root, "空目录").unwrap();
        folders::create(&database, &root, "有子目录/子").unwrap();
        let empty = folders::find_id(&database, "空目录").unwrap().unwrap();
        let parent = folders::find_id(&database, "有子目录").unwrap().unwrap();
        let root_id = folders::find_id(&database, "").unwrap().unwrap();

        assert_eq!(folders::deletion_blocker(&database, empty).unwrap(), None);
        assert_eq!(
            folders::deletion_blocker(&database, parent).unwrap(),
            None,
            "非空文件夹也能移入回收站"
        );
        assert!(
            folders::deletion_blocker(&database, root_id)
                .unwrap()
                .unwrap()
                .contains("根目录")
        );

        folders::trash(&database, parent).unwrap();
        assert!(
            folders::deletion_blocker(&database, parent)
                .unwrap()
                .unwrap()
                .contains("回收站")
        );
    }

    #[test]
    fn folder_operations_are_labelled_as_ai_writes() {
        let (_temp, root, database) = fixture();
        let (_, create_id) = folders::create_with_id(&database, &root, "作品/速写").unwrap();
        let (_, rename_id) = folders::rename_with_id(
            &database,
            &root,
            folders::find_id(&database, "作品/速写").unwrap().unwrap(),
            "线稿",
        )
        .unwrap();
        let target = folders::find_id(&database, "作品/线稿").unwrap().unwrap();
        let (_, delete_id) = folders::trash_with_id(&database, target).unwrap();

        for operation_id in [create_id, rename_id, delete_id] {
            mark_ai_origin(&database, operation_id);
            let label = operation_label(&database, operation_id.unwrap());
            assert!(
                label.starts_with("AI · "),
                "文件夹操作记录应当带 AI 前缀，实际是 {label}"
            );
            // 漏传 operationId 时不能报错，也不能影响后续写入。
            mark_ai_origin(&database, None);
        }
    }

    fn operation_label(database: &LibraryDb, operation_id: i64) -> String {
        database
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT label FROM operations WHERE id = ?1",
                    [operation_id],
                    |row| row.get(0),
                )?)
            })
            .unwrap()
    }
}

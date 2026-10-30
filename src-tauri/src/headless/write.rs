//! 写路径命令：导入、元数据、文件夹、集合、历史、查看器与编辑。
//!
//! 与 Tauri 命令共用 `AppState`/领域函数，只把参数换成 JSON DTO；批量任务在这里
//! 启动后同步等待终态，结果里同时给出任务快照与操作记录。

use std::cell::Cell;

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::Manager;

use crate::{
    app::{error::AppError, state::AppState, tasks::TaskStartResult, trace},
    import::ImportRequest,
    library::{
        collections::CombineMode,
        edit::{self, EditRequest},
        folders::FolderSortKey,
        viewer,
    },
};

use super::{NullReporter, Options, check_task, value_of, wait_task};

pub(super) fn dispatch(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let state = state.inner();
    match (options.command.as_str(), options.action.as_deref()) {
        ("import", Some("run")) => {
            let request: ImportRequest = options.request()?;
            import_run(app, request, true, cancelled)
        }
        ("import", Some("start")) => {
            let request: ImportRequest = options.request()?;
            import_run(app, request, false, cancelled)
        }
        ("import", Some("cancel")) => {
            let request: JobRequest = options.request()?;
            trace::write("cancel_import", || {
                AppError::command(state.cancel_import(&request.job_id))
            })?;
            Ok(json!({ "jobId": request.job_id, "cancelRequested": true }))
        }
        ("import", Some("status")) => {
            let request: JobRequest = options.request()?;
            value_of(trace::read("get_import_status", || {
                Ok(state.import_receipt(&request.job_id))
            }))
        }
        ("metadata", Some("score")) => {
            let request: ScoreRequest = options.request()?;
            value_of(trace::write("update_image_score", || {
                AppError::command(state.update_image_score(request.image_id, request.score))
            }))
            .map(|_| json!({ "updated": true }))
        }
        ("metadata", Some("set-score")) => {
            let request: SetScoreRequest = options.request()?;
            value_of(trace::write("set_images_score", || {
                AppError::command(state.set_images_score(&request.image_ids, request.score))
            }))
        }
        ("metadata", Some("description")) => {
            let request: DescriptionRequest = options.request()?;
            value_of(trace::write("update_image_description", || {
                AppError::command(
                    state.update_image_description(request.image_id, &request.description),
                )
            }))
            .map(|_| json!({ "updated": true }))
        }
        ("metadata", Some("tags")) => {
            let request: ImageTagsSetRequest = options.request()?;
            value_of(trace::write("set_image_tags", || {
                AppError::command(state.set_image_tags(request.image_id, &request.tag_names))
            }))
        }
        ("metadata", Some("add-tags")) => {
            let request: ImageTagsRequest = options.request()?;
            value_of(trace::write("add_tags_to_images", || {
                AppError::command(state.add_tags(&request.image_ids, &request.tag_names))
            }))
        }
        ("metadata", Some("remove-tags")) => {
            let request: ImageTagsRequest = options.request()?;
            value_of(trace::write("remove_tags_from_images", || {
                AppError::command(state.remove_tags(&request.image_ids, &request.tag_names))
            }))
        }
        ("metadata", Some("rename-tag")) => {
            let request: RenameTagRequest = options.request()?;
            value_of(trace::write("rename_tag", || {
                AppError::command(state.rename_tag(request.tag_id, &request.name))
            }))
            .map(|_| json!({ "renamed": true }))
        }
        ("metadata", Some("delete-tag")) => {
            let request: TagRequest = options.request()?;
            value_of(trace::write("delete_tag", || {
                AppError::command(state.delete_tag(request.tag_id))
            }))
        }
        ("metadata", Some("merge-tags")) => {
            let request: MergeTagsRequest = options.request()?;
            value_of(trace::write("merge_tags", || {
                AppError::command(state.merge_tags(request.source_id, request.target_id))
            }))
        }
        ("metadata", Some("move")) => {
            let request: MoveRequest = options.request()?;
            value_of(trace::write("move_images_to_folder", || {
                AppError::command(state.move_images(
                    &request.image_ids,
                    &request.folder_path,
                    &NullReporter,
                ))
            }))
        }
        ("metadata", Some("trash")) => {
            let request: ImageIdsRequest = options.request()?;
            value_of(trace::write("trash_images", || {
                AppError::command(state.trash_images(&request.image_ids))
            }))
        }
        ("metadata", Some("restore")) => {
            let request: ImageIdsRequest = options.request()?;
            value_of(trace::write("restore_images", || {
                AppError::command(state.restore_images(&request.image_ids))
            }))
        }
        ("metadata", Some("delete")) => {
            let request: ImageIdsRequest = options.request()?;
            value_of(trace::write("delete_images_permanently", || {
                AppError::command(state.delete_images(&request.image_ids, &NullReporter))
            }))
        }
        ("metadata", Some("empty-trash")) => value_of(trace::write("empty_trash", || {
            AppError::command(state.empty_trash(&NullReporter))
        })),
        ("folder", Some("create")) => {
            let request: FolderPathRequest = options.request()?;
            value_of(trace::write("create_folder", || {
                AppError::command(state.create_folder(&request.path))
            }))
        }
        ("folder", Some("rename")) => {
            let request: FolderRenameRequest = options.request()?;
            value_of(trace::write("rename_folder", || {
                AppError::command(state.rename_folder(request.folder_id, &request.name))
            }))
        }
        ("folder", Some("move")) => {
            let request: FolderMoveRequest = options.request()?;
            value_of(trace::write("move_folder", || {
                AppError::command(state.move_folder(request.folder_id, request.parent_id))
            }))
        }
        ("folder", Some("delete")) => {
            let request: FolderIdRequest = options.request()?;
            value_of(trace::write("delete_folder", || {
                AppError::command(state.delete_folder(request.folder_id))
            }))
        }
        ("folder", Some("restore")) => {
            let request: FolderIdRequest = options.request()?;
            value_of(trace::write("restore_folder", || {
                AppError::command(state.restore_folder(request.folder_id))
            }))
        }
        ("folder", Some("delete-permanently")) => {
            let request: FolderIdRequest = options.request()?;
            value_of(trace::write("delete_folder_permanently", || {
                AppError::command(state.delete_folder_permanently(request.folder_id))
            }))
        }
        ("folder", Some("reorder")) => {
            let request: FolderReorderRequest = options.request()?;
            value_of(trace::write("reorder_folders", || {
                AppError::command(state.reorder_folders(
                    &request.folder_ids,
                    request.parent_id,
                    request.sort_index,
                ))
            }))
        }
        ("folder", Some("place")) => {
            let request: FolderPlaceRequest = options.request()?;
            value_of(trace::write("place_folders", || {
                AppError::command(state.place_folders(
                    &request.folder_ids,
                    request.parent_id,
                    request.sort_index,
                ))
            }))
        }
        ("folder", Some("apply-order")) => {
            let request: FolderApplyOrderRequest = options.request()?;
            let orders: Vec<(i64, Vec<i64>)> = request
                .orders
                .into_iter()
                .map(|order| (order.parent_id, order.folder_ids))
                .collect();
            value_of(trace::write("apply_folder_order", || {
                AppError::command(state.apply_folder_order(
                    request.folder_id,
                    request.key,
                    request.descending,
                    request.recursive,
                    &orders,
                ))
            }))
        }
        ("folder", Some("trash")) => value_of(trace::read("get_trash_folders", || {
            AppError::command(state.trash_folders())
        })),
        ("collection", Some("list")) => value_of(trace::read("get_collections", || {
            AppError::command(state.collections())
        })),
        ("collection", Some("create")) => {
            let request: NameRequest = options.request()?;
            value_of(trace::write("create_collection", || {
                AppError::command(state.create_collection(&request.name))
            }))
        }
        ("collection", Some("rename")) => {
            let request: CollectionRenameRequest = options.request()?;
            value_of(trace::write("rename_collection", || {
                AppError::command(state.rename_collection(request.collection_id, &request.name))
            }))
            .map(|_| json!({ "renamed": true }))
        }
        ("collection", Some("delete")) => {
            let request: CollectionIdRequest = options.request()?;
            value_of(trace::write("delete_collection", || {
                AppError::command(state.delete_collection(request.collection_id))
            }))
        }
        ("collection", Some("reorder")) => {
            let request: CollectionReorderRequest = options.request()?;
            value_of(trace::write("reorder_collections", || {
                AppError::command(state.reorder_collections(&request.collection_ids))
            }))
            .map(|_| json!({ "reordered": true }))
        }
        ("collection", Some("add")) => {
            let request: CollectionImagesRequest = options.request()?;
            value_of(trace::write("add_images_to_collection", || {
                AppError::command(
                    state.add_images_to_collection(request.collection_id, &request.image_ids),
                )
            }))
        }
        ("collection", Some("remove")) => {
            let request: CollectionImagesRequest = options.request()?;
            value_of(trace::write("remove_images_from_collection", || {
                AppError::command(
                    state.remove_images_from_collection(request.collection_id, &request.image_ids),
                )
            }))
        }
        ("collection", Some("combine")) => {
            let request: CombineRequest = options.request()?;
            value_of(trace::write("combine_collections", || {
                AppError::command(state.combine_collections(
                    request.mode,
                    &request.collection_ids,
                    &request.name,
                ))
            }))
        }
        ("history", Some("list")) => value_of(trace::read("get_operation_history", || {
            AppError::command(state.history_page())
        })),
        ("history", Some("undo")) => {
            let request: TargetRequest = options.request()?;
            let start = trace::write("undo_to_operation", || {
                AppError::command(state.undo_to(request.target_id))
            })?;
            finish_history_task(state, start, cancelled, "撤销")
        }
        ("history", Some("redo")) => {
            let request: TargetRequest = options.request()?;
            let start = trace::write("redo_to_operation", || {
                AppError::command(state.redo_to(request.target_id))
            })?;
            finish_history_task(state, start, cancelled, "重做")
        }
        ("history", Some("clear")) => {
            let start = trace::write("clear_operation_history", || {
                AppError::command(state.clear_history())
            })?;
            finish_history_task(state, start, cancelled, "清空操作记录")
        }
        ("viewer", Some("source")) => {
            let request: ImageIdRequest = options.request()?;
            value_of(trace::read("get_viewer_source", || {
                let (database, root) = state
                    .current_library()
                    .ok_or_else(|| "尚未打开图库".to_owned())?;
                viewer::source(
                    &database,
                    &root,
                    request.image_id,
                    &state.resources(),
                    &state.images(),
                    &state.memory(),
                )
                .map_err(|error| error.to_string())
            }))
        }
        ("edit", Some("estimate")) => {
            let request: EditRequest = options.request()?;
            value_of(trace::read("estimate_edit_size", || {
                let (database, root) = state
                    .current_library()
                    .ok_or_else(|| "尚未打开图库".to_owned())?;
                edit::estimate(&database, &root, &request).map_err(|error| error.to_string())
            }))
        }
        ("edit", Some("save")) => {
            let request: EditRequest = options.request()?;
            value_of(trace::write("save_image_edit", || {
                // 先占住这张图的编辑权，再等图像预算；顺序与 GUI 命令一致。
                let _editing = state
                    .begin_edit(request.image_id)
                    .map_err(|error| error.to_string())?;
                let (database, thumbs, root) =
                    state.edit_handles().map_err(|error| error.to_string())?;
                let _permit = state
                    .images()
                    .acquire(&|| false)
                    .map_err(|_| "图像预算等待已取消".to_owned())?;
                let result = edit::save(&database, &root, &thumbs, &request)
                    .map_err(|error| error.to_string())?;
                state.after_history_write();
                Ok(result)
            }))
        }
        (
            "import" | "metadata" | "folder" | "collection" | "history" | "viewer" | "edit",
            action,
        ) => Err(match action {
            Some(action) => format!("未知命令：{} {action}", options.command),
            None => format!("未知命令：{}", options.command),
        }),
        _ => unreachable!("mod.rs 只把本组命令交给这里"),
    }
}

fn import_run(
    app: &tauri::AppHandle,
    request: ImportRequest,
    wait: bool,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let snapshot = trace::write("start_import", || {
        AppError::command(state.start_import(app.clone(), request))
    })?;
    let job_id = snapshot.progress.job_id.clone();
    let task = if wait {
        let task = wait_task(&state, &job_id)?;
        check_task(&task, cancelled)?;
        Some(task)
    } else {
        None
    };
    let job = state
        .import_receipt(&job_id)
        .ok_or_else(|| "导入任务已结束但没有回执".to_owned())?;
    let counts = AppError::command(state.counts())?;
    Ok(json!({ "job": job, "task": task, "counts": counts }))
}

fn finish_history_task(
    state: &AppState,
    start: TaskStartResult,
    cancelled: &Cell<bool>,
    label: &str,
) -> Result<Value, String> {
    let TaskStartResult::Started { task } = start else {
        return Err(format!("{label}被占用，稍后重试"));
    };
    let task = wait_task(state, &task.task_id)?;
    check_task(&task, cancelled)?;
    let history = AppError::command(state.history_page())?;
    Ok(json!({ "task": task, "history": history }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobRequest {
    job_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScoreRequest {
    image_id: i64,
    score: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetScoreRequest {
    image_ids: Vec<i64>,
    score: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DescriptionRequest {
    image_id: i64,
    description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageTagsSetRequest {
    image_id: i64,
    tag_names: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageTagsRequest {
    image_ids: Vec<i64>,
    tag_names: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TagRequest {
    tag_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameTagRequest {
    tag_id: i64,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeTagsRequest {
    source_id: i64,
    target_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveRequest {
    image_ids: Vec<i64>,
    folder_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageIdsRequest {
    image_ids: Vec<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderPathRequest {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderRenameRequest {
    folder_id: i64,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderMoveRequest {
    folder_id: i64,
    parent_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderIdRequest {
    folder_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderReorderRequest {
    folder_ids: Vec<i64>,
    parent_id: i64,
    sort_index: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderPlaceRequest {
    folder_ids: Vec<i64>,
    parent_id: i64,
    #[serde(default)]
    sort_index: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderApplyOrderRequest {
    folder_id: i64,
    key: FolderSortKey,
    descending: bool,
    recursive: bool,
    #[serde(default)]
    orders: Vec<FolderOrderInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderOrderInput {
    parent_id: i64,
    folder_ids: Vec<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameRequest {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionRenameRequest {
    collection_id: i64,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionIdRequest {
    collection_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionReorderRequest {
    collection_ids: Vec<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionImagesRequest {
    collection_id: i64,
    image_ids: Vec<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CombineRequest {
    mode: CombineMode,
    collection_ids: Vec<i64>,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetRequest {
    target_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageIdRequest {
    image_id: i64,
}

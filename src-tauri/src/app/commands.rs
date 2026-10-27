use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager, State, WebviewWindow};

use crate::{
    ai::runtime::{
        AiDescriptionResult, AiStatus, ImageSimilarityResult, SemanticSearchResult,
        SimilarityGroupsResult, SimilarityPage,
    },
    app::{
        error::{AppError, AppResult},
        settings::Settings,
        state::AppState,
        tasks::{
            BatchTaskRequest, LaneGuard, TaskKind, TaskPhase, TaskScope, TaskSnapshot,
            TaskStartResult,
        },
    },
    import::{ImportRequest, job::ImportSnapshot},
    library::{
        BatchReporter,
        capability::{FeatureSummary, ImageCapabilities},
        collections::{CollectionInfo, CombineMode},
        edit::{self, EditRequest, EditResult, EditSizeEstimate},
        folders::FolderSortKey,
        history::HistoryPage,
        manifest::LibraryInspection,
        metadata::{DeleteReport, ImageDetails, ImageState, MoveReport, SelectionTags, TagRef},
        optimize_estimate::OptimizeEstimate,
        query::{FilterCounts, FolderInfo, ImagePage, LibraryQuery, TagInfo},
        service::LibraryInfo,
        viewer::{self, PoseData, ViewerSource},
    },
};

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.snapshot()
}

#[tauri::command]
pub fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<Settings, String> {
    let previous = state.settings.snapshot();
    let saved = AppError::command(state.settings.replace(settings))?;
    if previous.ai != saved.ai {
        state.ai.apply_settings(&previous.ai, &saved.ai);
    }
    if previous.mcp != saved.mcp {
        state.apply_mcp_settings(&saved.mcp);
    }
    state.after_history_write();
    Ok(saved)
}

#[tauri::command]
pub fn get_mcp_status(state: State<'_, AppState>) -> crate::mcp::runtime::McpStatus {
    state.mcp_status()
}

#[tauri::command]
pub fn list_mcp_approvals(state: State<'_, AppState>) -> Vec<crate::mcp::approval::ApprovalView> {
    state.mcp_approval_views()
}

#[tauri::command]
pub async fn approve_mcp_approval(app: AppHandle, approval_id: String) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state
            .approve_mcp_approval(&approval_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn reject_mcp_approval(state: State<'_, AppState>, approval_id: String) -> Result<(), String> {
    AppError::command(state.reject_mcp_approval(&approval_id))
}

#[tauri::command]
pub fn inspect_library_path(state: State<'_, AppState>, path: String) -> LibraryInspection {
    state.inspect_library(&PathBuf::from(path))
}

#[tauri::command]
pub fn create_library(
    state: State<'_, AppState>,
    name: String,
    path: String,
) -> Result<LibraryInfo, String> {
    AppError::command(state.create_library(&PathBuf::from(path), &name))
}

#[tauri::command]
pub fn open_library_path(state: State<'_, AppState>, path: String) -> Result<LibraryInfo, String> {
    let path = PathBuf::from(path);
    match state.inspect_library(&path) {
        LibraryInspection::Library { .. } => AppError::command(state.open_library(&path)),
        LibraryInspection::Empty => Err("目录不是图库，请先创建图库".to_owned()),
        LibraryInspection::Invalid { reason } => Err(format!("该目录无法作为图库：{reason}")),
    }
}

#[tauri::command]
pub fn close_library(state: State<'_, AppState>) -> Result<(), String> {
    AppError::command(state.close_library())
}

#[tauri::command]
pub fn get_library_status(state: State<'_, AppState>) -> Result<Option<LibraryInfo>, String> {
    AppError::command(state.status())
}

#[tauri::command]
pub fn rename_library(state: State<'_, AppState>, name: String) -> Result<LibraryInfo, String> {
    AppError::command(state.rename_current_library(&name))
}

#[tauri::command]
pub fn window_minimize(window: WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn window_toggle_maximize(window: WebviewWindow) -> Result<bool, String> {
    let maximized = window.is_maximized().map_err(|error| error.to_string())?;
    if maximized {
        window.unmaximize().map_err(|error| error.to_string())?;
    } else {
        window.maximize().map_err(|error| error.to_string())?;
    }
    Ok(!maximized)
}

#[tauri::command]
pub fn window_is_maximized(window: WebviewWindow) -> Result<bool, String> {
    window.is_maximized().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn window_close(window: WebviewWindow) -> Result<(), String> {
    window.close().map_err(|error| error.to_string())
}

/// 设置语义：写入后回读真实状态，系统快捷键改变全屏时前端也能同步。
#[tauri::command]
pub fn window_set_fullscreen(window: WebviewWindow, value: bool) -> Result<bool, String> {
    window
        .set_fullscreen(value)
        .map_err(|error| error.to_string())?;
    window.is_fullscreen().map_err(|error| error.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub settings_path: String,
    pub log_path: String,
    /// 当前进程的可执行文件；开发模式与便携模式都能据此拼出 stdio 客户端配置。
    pub exe_path: String,
}

/// 日志文件名与 `lib.rs` 里 `TargetKind::Folder` 的 `file_name` 一致。
const LOG_FILE_STEM: &str = "lilith-artbank";

fn log_file_path() -> String {
    let dir = crate::app::paths::log_dir();
    // 插件按给定文件名落盘，不同版本对扩展名的处理不一致，两个候选都试一次。
    for candidate in [
        dir.join(format!("{LOG_FILE_STEM}.log")),
        dir.join(LOG_FILE_STEM),
    ] {
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    dir.join(format!("{LOG_FILE_STEM}.log"))
        .to_string_lossy()
        .into_owned()
}

#[tauri::command]
pub fn get_app_info(state: State<'_, AppState>) -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        settings_path: state.settings.path().to_string_lossy().into_owned(),
        log_path: log_file_path(),
        exe_path: std::env::current_exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// 在系统文件管理器里定位一个文件（选中它）或直接打开一个目录。
#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    let target = PathBuf::from(path.trim());
    if target.as_os_str().is_empty() {
        return Err("路径为空".to_owned());
    }
    let is_file = target.is_file();
    let dir = if target.is_dir() {
        target.clone()
    } else {
        target
            .parent()
            .map(|parent| parent.to_path_buf())
            .ok_or_else(|| "路径无效".to_owned())?
    };
    if !dir.exists() {
        return Err("目录不存在".to_owned());
    }

    #[cfg(target_os = "windows")]
    {
        let mut command = std::process::Command::new("explorer.exe");
        if is_file {
            // /select 需要真实文件路径；文件被移走时退化为打开父目录。
            command.arg(format!("/select,{}", target.display()));
        } else {
            command.arg(dir.as_os_str());
        }
        command.spawn().map_err(|error| error.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        if is_file {
            command.arg("-R");
            command.arg(target.as_os_str());
        } else {
            command.arg(dir.as_os_str());
        }
        command.spawn().map_err(|error| error.to_string())?;
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(dir.as_os_str())
            .spawn()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn start_import(
    app: AppHandle,
    state: State<'_, AppState>,
    request: ImportRequest,
) -> Result<ImportSnapshot, String> {
    AppError::command(state.start_import(app, request))
}

#[tauri::command]
pub fn cancel_import(state: State<'_, AppState>, job_id: String) -> Result<(), String> {
    AppError::command(state.cancel_import(&job_id))
}

#[tauri::command]
pub fn get_import_status(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<Option<ImportSnapshot>, String> {
    Ok(state.import_status(&job_id))
}

#[tauri::command]
pub fn copy_text(text: String) -> Result<(), String> {
    AppError::command(crate::platform::clipboard::copy_text(&text))
}

#[tauri::command]
pub fn query_images(state: State<'_, AppState>, query: LibraryQuery) -> Result<ImagePage, String> {
    AppError::command(state.query_images(query))
}

#[tauri::command]
pub fn query_image_ids(
    state: State<'_, AppState>,
    query: LibraryQuery,
) -> Result<Vec<i64>, String> {
    AppError::command(state.query_image_ids(query))
}

#[tauri::command]
pub fn get_filter_counts(
    state: State<'_, AppState>,
    query: LibraryQuery,
) -> Result<FilterCounts, String> {
    AppError::command(state.filter_counts(query))
}

#[tauri::command]
pub fn get_query_total(state: State<'_, AppState>, query: LibraryQuery) -> Result<u64, String> {
    AppError::command(state.query_total(query))
}

#[tauri::command]
pub fn get_folders(state: State<'_, AppState>) -> Result<Vec<FolderInfo>, String> {
    AppError::command(state.folders())
}

#[tauri::command]
pub fn create_folder(state: State<'_, AppState>, path: String) -> Result<Vec<FolderInfo>, String> {
    AppError::command(state.create_folder(&path))
}

#[tauri::command]
pub fn rename_folder(
    state: State<'_, AppState>,
    folder_id: i64,
    name: String,
) -> Result<Vec<FolderInfo>, String> {
    AppError::command(state.rename_folder(folder_id, &name))
}

#[tauri::command]
pub fn move_folder(
    state: State<'_, AppState>,
    folder_id: i64,
    parent_id: i64,
) -> Result<Vec<FolderInfo>, String> {
    AppError::command(state.move_folder(folder_id, parent_id))
}

#[tauri::command]
pub fn reorder_folders(
    state: State<'_, AppState>,
    folder_ids: Vec<i64>,
    parent_id: i64,
    sort_index: i64,
) -> Result<Vec<FolderInfo>, String> {
    AppError::command(state.reorder_folders(&folder_ids, parent_id, sort_index))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderOrderInput {
    parent_id: i64,
    folder_ids: Vec<i64>,
}

#[tauri::command]
pub fn place_folders(
    state: State<'_, AppState>,
    folder_ids: Vec<i64>,
    parent_id: i64,
    sort_index: Option<i64>,
) -> Result<Vec<FolderInfo>, String> {
    AppError::command(state.place_folders(&folder_ids, parent_id, sort_index))
}

#[tauri::command]
pub fn apply_folder_order(
    state: State<'_, AppState>,
    folder_id: i64,
    key: FolderSortKey,
    descending: bool,
    recursive: bool,
    orders: Vec<FolderOrderInput>,
) -> Result<Vec<FolderInfo>, String> {
    let orders: Vec<(i64, Vec<i64>)> = orders
        .into_iter()
        .map(|order| (order.parent_id, order.folder_ids))
        .collect();
    AppError::command(state.apply_folder_order(folder_id, key, descending, recursive, &orders))
}

#[tauri::command]
pub fn delete_folder(
    state: State<'_, AppState>,
    folder_id: i64,
) -> Result<Vec<FolderInfo>, String> {
    AppError::command(state.delete_folder(folder_id))
}

#[tauri::command]
pub fn get_tags(state: State<'_, AppState>, limit: Option<u32>) -> Result<Vec<TagInfo>, String> {
    AppError::command(state.tags(limit))
}

#[tauri::command]
pub fn search_tags(
    state: State<'_, AppState>,
    query: String,
    limit: Option<u32>,
) -> Result<Vec<TagInfo>, String> {
    AppError::command(state.search_tags(&query, limit))
}

#[tauri::command]
pub fn get_image_detail(state: State<'_, AppState>, image_id: i64) -> Result<ImageDetails, String> {
    AppError::command(state.image_detail(image_id))
}

#[tauri::command]
pub fn get_image_states(
    state: State<'_, AppState>,
    image_ids: Vec<i64>,
) -> Result<Vec<ImageState>, String> {
    AppError::command(state.image_states(&image_ids))
}

#[tauri::command]
pub fn get_selection_tags(
    state: State<'_, AppState>,
    image_ids: Vec<i64>,
) -> Result<SelectionTags, String> {
    AppError::command(state.selection_tags(&image_ids))
}

#[tauri::command]
pub fn update_image_score(
    state: State<'_, AppState>,
    image_id: i64,
    score: i64,
) -> Result<(), String> {
    AppError::command(state.update_image_score(image_id, score))
}

#[tauri::command]
pub fn set_images_score(
    state: State<'_, AppState>,
    image_ids: Vec<i64>,
    score: i64,
) -> Result<u64, String> {
    AppError::command(state.set_images_score(&image_ids, score))
}

#[tauri::command]
pub fn update_image_description(
    state: State<'_, AppState>,
    image_id: i64,
    description: String,
) -> Result<(), String> {
    AppError::command(state.update_image_description(image_id, &description))
}

#[tauri::command]
pub fn set_image_tags(
    state: State<'_, AppState>,
    image_id: i64,
    tag_names: Vec<String>,
) -> Result<Vec<TagRef>, String> {
    AppError::command(state.set_image_tags(image_id, &tag_names))
}

#[tauri::command]
pub fn add_tags_to_images(
    state: State<'_, AppState>,
    image_ids: Vec<i64>,
    tag_names: Vec<String>,
) -> Result<u64, String> {
    AppError::command(state.add_tags(&image_ids, &tag_names))
}

#[tauri::command]
pub fn remove_tags_from_images(
    state: State<'_, AppState>,
    image_ids: Vec<i64>,
    tag_names: Vec<String>,
) -> Result<u64, String> {
    AppError::command(state.remove_tags(&image_ids, &tag_names))
}

#[tauri::command]
pub fn rename_tag(state: State<'_, AppState>, tag_id: i64, name: String) -> Result<(), String> {
    AppError::command(state.rename_tag(tag_id, &name))
}

#[tauri::command]
pub fn delete_tag(state: State<'_, AppState>, tag_id: i64) -> Result<u64, String> {
    AppError::command(state.delete_tag(tag_id))
}

/// 在 write lane 上跑一个有进度的批量操作：占用、进度、终态都交给任务总线。
async fn run_write_task<T, F>(
    app: AppHandle,
    kind: TaskKind,
    scope: String,
    work: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&AppState, &dyn BatchReporter) -> AppResult<T> + Send + 'static,
{
    let (handle, guard) = {
        let state = app.state::<AppState>();
        state
            .begin_write_task(&app, kind, scope)
            .map_err(|error| error.to_string())?
    };
    tauri::async_runtime::spawn_blocking(move || {
        let _guard: LaneGuard = guard;
        handle.set_phase(TaskPhase::Running);
        let state = app.state::<AppState>();
        let result = work(&state, &*handle);
        handle.finish_from(&result);
        result.map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn move_images_to_folder(
    app: AppHandle,
    image_ids: Vec<i64>,
    folder_path: String,
) -> Result<MoveReport, String> {
    let scope = format!("{} 张", image_ids.len());
    run_write_task(app, TaskKind::FolderMove, scope, move |state, reporter| {
        state.move_images(&image_ids, &folder_path, reporter)
    })
    .await
}

#[tauri::command]
pub fn trash_images(state: State<'_, AppState>, image_ids: Vec<i64>) -> Result<u64, String> {
    AppError::command(state.trash_images(&image_ids))
}

#[tauri::command]
pub fn restore_images(state: State<'_, AppState>, image_ids: Vec<i64>) -> Result<u64, String> {
    AppError::command(state.restore_images(&image_ids))
}

#[tauri::command]
pub async fn delete_images_permanently(
    app: AppHandle,
    image_ids: Vec<i64>,
) -> Result<DeleteReport, String> {
    let scope = format!("{} 张", image_ids.len());
    run_write_task(
        app,
        TaskKind::PermanentDelete,
        scope,
        move |state, reporter| state.delete_images(&image_ids, reporter),
    )
    .await
}

#[tauri::command]
pub async fn empty_trash(app: AppHandle) -> Result<DeleteReport, String> {
    run_write_task(
        app,
        TaskKind::EmptyTrash,
        "回收站".to_owned(),
        |state, reporter| state.empty_trash(reporter),
    )
    .await
}

#[tauri::command]
pub fn get_operation_history(state: State<'_, AppState>) -> Result<HistoryPage, String> {
    AppError::command(state.history_page())
}

#[tauri::command]
pub fn undo_to_operation(
    state: State<'_, AppState>,
    target_id: i64,
) -> Result<TaskStartResult, String> {
    AppError::command(state.undo_to(target_id))
}

#[tauri::command]
pub fn redo_to_operation(
    state: State<'_, AppState>,
    target_id: i64,
) -> Result<TaskStartResult, String> {
    AppError::command(state.redo_to(target_id))
}

#[tauri::command]
pub fn clear_operation_history(state: State<'_, AppState>) -> Result<TaskStartResult, String> {
    AppError::command(state.clear_history())
}

#[tauri::command]
pub fn get_collections(state: State<'_, AppState>) -> Result<Vec<CollectionInfo>, String> {
    AppError::command(state.collections())
}

#[tauri::command]
pub fn create_collection(
    state: State<'_, AppState>,
    name: String,
) -> Result<CollectionInfo, String> {
    AppError::command(state.create_collection(&name))
}

#[tauri::command]
pub fn rename_collection(
    state: State<'_, AppState>,
    collection_id: i64,
    name: String,
) -> Result<(), String> {
    AppError::command(state.rename_collection(collection_id, &name))
}

#[tauri::command]
pub fn delete_collection(state: State<'_, AppState>, collection_id: i64) -> Result<u64, String> {
    AppError::command(state.delete_collection(collection_id))
}

#[tauri::command]
pub fn reorder_collections(
    state: State<'_, AppState>,
    collection_ids: Vec<i64>,
) -> Result<(), String> {
    AppError::command(state.reorder_collections(&collection_ids))
}

#[tauri::command]
pub fn add_images_to_collection(
    state: State<'_, AppState>,
    collection_id: i64,
    image_ids: Vec<i64>,
) -> Result<u64, String> {
    AppError::command(state.add_images_to_collection(collection_id, &image_ids))
}

#[tauri::command]
pub fn remove_images_from_collection(
    state: State<'_, AppState>,
    collection_id: i64,
    image_ids: Vec<i64>,
) -> Result<u64, String> {
    AppError::command(state.remove_images_from_collection(collection_id, &image_ids))
}

#[tauri::command]
pub fn combine_collections(
    state: State<'_, AppState>,
    mode: CombineMode,
    collection_ids: Vec<i64>,
    name: String,
) -> Result<CollectionInfo, String> {
    AppError::command(state.combine_collections(mode, &collection_ids, &name))
}

#[tauri::command]
pub fn create_collections_from_groups(
    state: State<'_, AppState>,
    groups: Vec<Vec<i64>>,
    base_name: String,
) -> Result<Vec<CollectionInfo>, String> {
    AppError::command(state.create_collections_from_groups(&groups, &base_name))
}

#[tauri::command]
pub async fn get_viewer_source(
    state: State<'_, AppState>,
    image_id: i64,
) -> Result<ViewerSource, String> {
    let (database, root) = state
        .current_library()
        .ok_or_else(|| "尚未打开图库".to_owned())?;
    tauri::async_runtime::spawn_blocking(move || viewer::source(&database, &root, image_id))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn save_image_edit(
    state: State<'_, AppState>,
    request: EditRequest,
) -> Result<EditResult, String> {
    let guard = state.begin_edit().map_err(|error| error.to_string())?;
    let (database, thumbs, root) = state.edit_handles().map_err(|error| error.to_string())?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        edit::save(&database, &root, &thumbs, &request)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    state.after_history_write();
    Ok(result)
}

#[tauri::command]
pub async fn estimate_edit_size(
    state: State<'_, AppState>,
    request: EditRequest,
) -> Result<EditSizeEstimate, String> {
    let (database, root) = state
        .current_library()
        .ok_or_else(|| "尚未打开图库".to_owned())?;
    tauri::async_runtime::spawn_blocking(move || edit::estimate(&database, &root, &request))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_image_pose(
    state: State<'_, AppState>,
    image_id: i64,
) -> Result<Option<PoseData>, String> {
    AppError::command(state.image_pose(image_id))
}

#[tauri::command]
pub async fn get_ai_status(state: State<'_, AppState>) -> Result<AiStatus, String> {
    let settings = state.settings.snapshot().ai;
    let runtime = state.ai.clone();
    tauri::async_runtime::spawn_blocking(move || runtime.status(&settings))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn ai_start_service(state: State<'_, AppState>) -> Result<(), String> {
    let settings = state.settings.snapshot().ai;
    AppError::command(state.ai.start_service(&settings))
}

#[tauri::command]
pub async fn ai_stop_service(state: State<'_, AppState>) -> Result<(), String> {
    let settings = state.settings.snapshot().ai;
    let runtime = state.ai.clone();
    // 停止包含 shutdown 请求与等待进程退出，放到阻塞线程池避免卡住界面。
    tauri::async_runtime::spawn_blocking(move || runtime.stop_service(&settings))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn start_batch_task(
    app: AppHandle,
    state: State<'_, AppState>,
    request: BatchTaskRequest,
) -> Result<TaskStartResult, String> {
    AppError::command(state.start_batch_task(app, request))
}

#[tauri::command]
pub fn list_tasks(state: State<'_, AppState>) -> Vec<TaskSnapshot> {
    state.tasks_snapshot()
}

#[tauri::command]
pub fn cancel_task(state: State<'_, AppState>, task_id: String) -> Result<(), String> {
    AppError::command(state.cancel_task(&task_id))
}

#[tauri::command]
pub fn get_similarity_groups_result(
    state: State<'_, AppState>,
    task_id: String,
) -> Option<SimilarityGroupsResult> {
    state.similarity_groups_result(&task_id)
}

#[tauri::command]
pub fn get_optimize_estimate(
    state: State<'_, AppState>,
    task_id: String,
) -> Option<OptimizeEstimate> {
    state.optimize_estimate(&task_id)
}

#[tauri::command]
pub fn get_image_capabilities(
    state: State<'_, AppState>,
    image_ids: Vec<i64>,
) -> Result<Vec<ImageCapabilities>, String> {
    AppError::command(state.image_capabilities(&image_ids))
}

#[tauri::command]
pub fn get_feature_summary(state: State<'_, AppState>) -> Result<FeatureSummary, String> {
    AppError::command(state.feature_summary())
}

#[tauri::command]
pub async fn ai_describe_image(
    state: State<'_, AppState>,
    image_id: i64,
    prompt: Option<String>,
    max_tokens: Option<u32>,
) -> Result<AiDescriptionResult, String> {
    let settings = state.settings.snapshot().ai;
    let runtime = state.ai.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        runtime.describe_image(&settings, image_id, prompt.as_deref(), max_tokens)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    state.after_history_write();
    Ok(result)
}

#[tauri::command]
pub async fn ai_generate_pose(
    state: State<'_, AppState>,
    image_id: i64,
) -> Result<PoseData, String> {
    let settings = state.settings.snapshot().ai;
    let runtime = state.ai.clone();
    tauri::async_runtime::spawn_blocking(move || runtime.generate_pose(&settings, image_id))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn semantic_search(
    state: State<'_, AppState>,
    text: String,
    limit: Option<usize>,
) -> Result<SemanticSearchResult, String> {
    let settings = state.settings.snapshot().ai;
    let runtime = state.ai.clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.semantic_search(&settings, &text, limit, None, None)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn find_similar_images(
    state: State<'_, AppState>,
    kind: String,
    target_ids: Vec<i64>,
    threshold: Option<f64>,
    limit: Option<usize>,
    scope: Option<TaskScope>,
) -> Result<ImageSimilarityResult, String> {
    let runtime = state.ai.clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.find_similar_images(&kind, &target_ids, threshold, limit, scope.as_ref())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_similarity_page(
    state: State<'_, AppState>,
    token: String,
    offset: usize,
    limit: Option<usize>,
) -> Result<SimilarityPage, String> {
    let runtime = state.ai.clone();
    tauri::async_runtime::spawn_blocking(move || runtime.similarity_page(&token, offset, limit))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_similarity_ids(state: State<'_, AppState>, token: String) -> Result<Vec<i64>, String> {
    AppError::command(state.ai.similarity_ids(&token))
}

#[tauri::command]
pub async fn copy_image(state: State<'_, AppState>, image_id: i64) -> Result<(), String> {
    let path = state
        .copy_source(image_id)
        .map_err(|error| error.to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        crate::platform::clipboard::copy_image(&path).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

pub fn handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        get_settings,
        save_settings,
        inspect_library_path,
        create_library,
        open_library_path,
        close_library,
        get_library_status,
        rename_library,
        window_minimize,
        window_toggle_maximize,
        window_is_maximized,
        window_close,
        window_set_fullscreen,
        get_app_info,
        reveal_path,
        start_import,
        cancel_import,
        get_import_status,
        copy_text,
        query_images,
        query_image_ids,
        get_filter_counts,
        get_query_total,
        get_folders,
        create_folder,
        rename_folder,
        move_folder,
        reorder_folders,
        place_folders,
        apply_folder_order,
        delete_folder,
        get_tags,
        search_tags,
        get_image_detail,
        get_image_states,
        get_selection_tags,
        update_image_score,
        set_images_score,
        update_image_description,
        set_image_tags,
        add_tags_to_images,
        remove_tags_from_images,
        rename_tag,
        delete_tag,
        move_images_to_folder,
        trash_images,
        restore_images,
        delete_images_permanently,
        empty_trash,
        get_operation_history,
        undo_to_operation,
        redo_to_operation,
        clear_operation_history,
        get_collections,
        create_collection,
        rename_collection,
        delete_collection,
        reorder_collections,
        add_images_to_collection,
        remove_images_from_collection,
        combine_collections,
        create_collections_from_groups,
        get_viewer_source,
        save_image_edit,
        estimate_edit_size,
        get_image_pose,
        copy_image,
        get_ai_status,
        ai_start_service,
        ai_stop_service,
        start_batch_task,
        list_tasks,
        cancel_task,
        get_similarity_groups_result,
        get_optimize_estimate,
        get_image_capabilities,
        get_feature_summary,
        ai_describe_image,
        ai_generate_pose,
        semantic_search,
        find_similar_images,
        get_similarity_page,
        get_similarity_ids,
        get_mcp_status,
        list_mcp_approvals,
        approve_mcp_approval,
        reject_mcp_approval
    ]
}

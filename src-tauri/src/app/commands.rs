use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, State, WebviewWindow};

use crate::{
    ai::{
        job::{AiJobRequest, AiJobSnapshot},
        runtime::{
            AiDescriptionResult, AiStatus, ImageSimilarityResult, SemanticSearchResult,
            SimilarityGroupsResult, SimilarityPage,
        },
    },
    app::{
        bench::{self, ThumbBenchInfo, ThumbReadStats},
        error::AppError,
        settings::Settings,
        state::AppState,
    },
    import::{ImportRequest, job::ImportSnapshot},
    library::{
        collections::{CollectionInfo, CombineMode},
        edit::{self, EditRequest, EditResult},
        manifest::LibraryInspection,
        metadata::{DeleteReport, ImageDetails, MoveReport, TagRef},
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
    AppError::command(state.settings.replace(settings))
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

#[tauri::command]
pub fn window_toggle_fullscreen(window: WebviewWindow) -> Result<bool, String> {
    let fullscreen = window.is_fullscreen().map_err(|error| error.to_string())?;
    window
        .set_fullscreen(!fullscreen)
        .map_err(|error| error.to_string())?;
    Ok(!fullscreen)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub settings_path: String,
}

#[tauri::command]
pub fn get_app_info(state: State<'_, AppState>) -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        settings_path: state.settings.path().to_string_lossy().into_owned(),
    }
}

#[tauri::command]
pub async fn thumb_bench_prepare(state: State<'_, AppState>) -> Result<ThumbBenchInfo, String> {
    if !cfg!(debug_assertions) {
        return Err("缩略图基准只在开发构建中可用".to_owned());
    }
    let (bench, info) = tauri::async_runtime::spawn_blocking(bench::prepare)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    state.set_thumb_bench(Some(bench));
    Ok(info)
}

#[tauri::command]
pub async fn thumb_bench_measure_read(
    state: State<'_, AppState>,
) -> Result<ThumbReadStats, String> {
    let bench = state
        .thumb_bench()
        .ok_or_else(|| "请先生成基准样本".to_owned())?;
    tauri::async_runtime::spawn_blocking(move || bench.measure_read(bench::READ_ROUNDS))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn thumb_bench_cleanup(state: State<'_, AppState>) -> Result<(), String> {
    state.set_thumb_bench(None);
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

#[tauri::command]
pub fn move_images_to_folder(
    state: State<'_, AppState>,
    image_ids: Vec<i64>,
    folder_path: String,
) -> Result<MoveReport, String> {
    AppError::command(state.move_images(&image_ids, &folder_path))
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
pub fn delete_images_permanently(
    state: State<'_, AppState>,
    image_ids: Vec<i64>,
) -> Result<DeleteReport, String> {
    AppError::command(state.delete_images(&image_ids))
}

#[tauri::command]
pub fn empty_trash(state: State<'_, AppState>) -> Result<DeleteReport, String> {
    AppError::command(state.empty_trash())
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
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        edit::save(&database, &root, &thumbs, &request).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
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
pub async fn ai_ensure_models(
    state: State<'_, AppState>,
    capabilities: Vec<String>,
) -> Result<AiStatus, String> {
    let settings = state.settings.snapshot().ai;
    let runtime = state.ai.clone();
    tauri::async_runtime::spawn_blocking(move || runtime.ensure_models(&settings, &capabilities))
        .await
        .map_err(|error| error.to_string())?
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
pub fn start_ai_job(
    app: AppHandle,
    state: State<'_, AppState>,
    request: AiJobRequest,
) -> Result<AiJobSnapshot, String> {
    let settings = state.settings.snapshot().ai;
    let request = request.normalized().map_err(|error| error.to_string())?;
    state
        .ai
        .start_job(app, &settings, request)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn cancel_ai_job(state: State<'_, AppState>, job_id: String) -> Result<(), String> {
    AppError::command(state.ai.cancel_job(&job_id))
}

#[tauri::command]
pub fn get_ai_job_status(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<Option<AiJobSnapshot>, String> {
    Ok(state.ai.job_status(&job_id))
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
    tauri::async_runtime::spawn_blocking(move || {
        runtime.describe_image(&settings, image_id, prompt.as_deref(), max_tokens)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
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
    tauri::async_runtime::spawn_blocking(move || runtime.semantic_search(&settings, &text, limit))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn find_similar_images(
    state: State<'_, AppState>,
    kind: String,
    target_id: i64,
    threshold: Option<u32>,
    limit: Option<usize>,
) -> Result<ImageSimilarityResult, String> {
    let runtime = state.ai.clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.find_similar_images(&kind, target_id, threshold, limit)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn find_similar_groups(
    state: State<'_, AppState>,
    kind: String,
    threshold: Option<u32>,
    similarity_threshold: Option<f32>,
) -> Result<SimilarityGroupsResult, String> {
    let runtime = state.ai.clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.find_similar_groups(&kind, threshold, similarity_threshold)
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
        window_toggle_fullscreen,
        get_app_info,
        thumb_bench_prepare,
        thumb_bench_measure_read,
        thumb_bench_cleanup,
        start_import,
        cancel_import,
        get_import_status,
        copy_text,
        query_images,
        query_image_ids,
        get_filter_counts,
        get_folders,
        create_folder,
        rename_folder,
        move_folder,
        delete_folder,
        get_tags,
        search_tags,
        get_image_detail,
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
        get_collections,
        create_collection,
        rename_collection,
        delete_collection,
        reorder_collections,
        add_images_to_collection,
        remove_images_from_collection,
        combine_collections,
        get_viewer_source,
        save_image_edit,
        get_image_pose,
        copy_image,
        get_ai_status,
        ai_ensure_models,
        ai_start_service,
        ai_stop_service,
        start_ai_job,
        cancel_ai_job,
        get_ai_job_status,
        ai_describe_image,
        ai_generate_pose,
        semantic_search,
        find_similar_images,
        find_similar_groups,
        get_similarity_page,
        get_similarity_ids
    ]
}

use std::path::PathBuf;

use serde::Serialize;
use tauri::{State, WebviewWindow};

use crate::{
    app::{error::AppError, settings::Settings, state::AppState},
    library::{manifest::LibraryInspection, service::LibraryInfo},
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
pub fn add_library_path(
    state: State<'_, AppState>,
    name: String,
    path: String,
) -> Result<LibraryInfo, String> {
    let path = PathBuf::from(path);
    match state.inspect_library(&path) {
        LibraryInspection::Library { .. } => AppError::command(state.open_library(&path)),
        LibraryInspection::Empty => AppError::command(state.create_library(&path, &name)),
        LibraryInspection::Invalid { reason } => Err(format!("该目录无法作为图库：{reason}")),
    }
}

#[tauri::command]
pub fn open_library(state: State<'_, AppState>, library_id: String) -> Result<LibraryInfo, String> {
    let settings = state.settings.snapshot();
    let entry = settings
        .libraries
        .iter()
        .find(|item| item.id == library_id)
        .ok_or_else(|| "图库不在列表中".to_owned())?;
    AppError::command(state.open_library(&PathBuf::from(&entry.path)))
}

#[tauri::command]
pub fn open_library_path(state: State<'_, AppState>, path: String) -> Result<LibraryInfo, String> {
    AppError::command(state.open_library(&PathBuf::from(path)))
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
pub fn remove_library(state: State<'_, AppState>, library_id: String) -> Result<Settings, String> {
    AppError::command(state.remove_library(&library_id))
}

#[tauri::command]
pub fn rename_library(
    state: State<'_, AppState>,
    library_id: String,
    name: String,
) -> Result<Settings, String> {
    AppError::command(state.rename_library(&library_id, &name))
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

pub fn handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        get_settings,
        save_settings,
        inspect_library_path,
        create_library,
        add_library_path,
        open_library,
        open_library_path,
        close_library,
        get_library_status,
        remove_library,
        rename_library,
        window_minimize,
        window_toggle_maximize,
        window_is_maximized,
        window_close,
        get_app_info
    ]
}

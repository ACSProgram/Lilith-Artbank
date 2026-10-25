use std::path::{Path, PathBuf};

/// 便携程序根目录：设置、日志与相对资源路径都以它为准。
///
/// release 下为 exe 所在目录；debug 下默认使用仓库根目录，便于直接跑
/// `npm run tauri dev`。两种模式都可以用 `LILITH_ARTBANK_ROOT` 覆盖。
pub fn app_root() -> PathBuf {
    if let Some(root) = std::env::var_os("LILITH_ARTBANK_ROOT")
        && !root.is_empty()
    {
        return PathBuf::from(root);
    }
    if cfg!(debug_assertions)
        && let Some(repo) = Path::new(env!("CARGO_MANIFEST_DIR")).parent()
    {
        return repo.to_path_buf();
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn settings_path() -> PathBuf {
    app_root().join("settings.json")
}

pub fn log_dir() -> PathBuf {
    app_root().join("logs")
}

pub fn ai_server_log_path() -> PathBuf {
    log_dir().join("ai-server.log")
}

/// 相对路径按程序根目录解析；绝对路径保持原样。
pub fn resolve(value: &str) -> PathBuf {
    let path = Path::new(value.trim());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        app_root().join(path)
    }
}

/// CLIP 模型目录：`clip_model` 可以相对模型根目录，也可以直接给绝对路径。
pub fn clip_model_dir(model_root: Option<&str>, clip_model: Option<&str>) -> Option<PathBuf> {
    let clip_model = clip_model
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let clip_path = Path::new(clip_model);
    if clip_path.is_absolute() {
        return Some(clip_path.to_path_buf());
    }
    let root = model_root
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(resolve)
        .unwrap_or_else(app_root);
    Some(root.join(clip_path))
}

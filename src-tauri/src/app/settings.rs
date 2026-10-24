use std::{
    io::Write,
    path::{Path, PathBuf},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::app::error::{AppError, AppResult};

pub const SETTINGS_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub settings_version: u32,
    pub library_path: Option<String>,
    pub ai: AiSettings,
    pub performance: PerformanceSettings,
    pub ui: UiSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            settings_version: SETTINGS_VERSION,
            library_path: None,
            ai: AiSettings::default(),
            performance: PerformanceSettings::default(),
            ui: UiSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AiSettings {
    pub base_url: String,
    pub executable_path: Option<String>,
    pub port: u16,
    pub auto_start: bool,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:18766".into(),
            executable_path: None,
            port: 18766,
            auto_start: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PerformanceSettings {
    /// 元数据读取连接数。写连接始终只有一条，避免 SQLite 写放大。
    pub reader_connections: u32,
    /// 为空时使用 CPU 可用并行度减去一个线程。
    pub worker_threads: Option<usize>,
    pub thumbnail_cache_mb: u32,
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        Self {
            reader_connections: 4,
            worker_threads: None,
            thumbnail_cache_mb: 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiSettings {
    pub theme: String,
    pub grid_thumb_px: u32,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "light".into(),
            grid_thumb_px: 220,
        }
    }
}

impl Settings {
    fn validate(&self) -> AppResult<()> {
        if self.settings_version != SETTINGS_VERSION {
            return Err(AppError::message("设置文件版本不受支持"));
        }
        if let Some(path) = &self.library_path {
            let path = path.trim();
            if path.is_empty() || !Path::new(path).is_absolute() {
                return Err(AppError::message("图库路径必须是非空的绝对路径"));
            }
        }
        let url = self.ai.base_url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(AppError::message(
                "AI 服务地址必须以 http:// 或 https:// 开头",
            ));
        }
        if self.ai.port == 0 {
            return Err(AppError::message("AI 服务端口无效"));
        }
        if !(1..=16).contains(&self.performance.reader_connections) {
            return Err(AppError::message("读取连接数必须在 1-16 之间"));
        }
        if let Some(threads) = self.performance.worker_threads
            && !(1..=128).contains(&threads)
        {
            return Err(AppError::message("工作线程数必须在 1-128 之间"));
        }
        if self.performance.thumbnail_cache_mb < 64 {
            return Err(AppError::message("缩略图缓存不能小于 64 MB"));
        }
        if !matches!(self.ui.theme.as_str(), "dark" | "light") {
            return Err(AppError::message("主题只能是 dark 或 light"));
        }
        if !(96..=512).contains(&self.ui.grid_thumb_px) {
            return Err(AppError::message("网格缩略图尺寸必须在 96-512 像素之间"));
        }
        Ok(())
    }
}

pub struct SettingsStore {
    path: PathBuf,
    value: Mutex<Settings>,
}

impl SettingsStore {
    pub fn load_or_create(path: PathBuf) -> AppResult<Self> {
        let settings = if path.is_file() {
            match std::fs::read(&path)
                .map_err(AppError::from)
                .and_then(|bytes| parse_settings(&bytes))
            {
                Ok(settings) => settings,
                Err(error) => {
                    let backup = path.with_extension(format!(
                        "bad-{}.json",
                        chrono::Local::now().format("%Y%m%d-%H%M%S")
                    ));
                    let _ = std::fs::rename(&path, &backup);
                    log::warn!("设置文件无法读取，已备份到 {}：{error}", backup.display());
                    Settings::default()
                }
            }
        } else {
            Settings::default()
        };

        settings.validate()?;
        let store = Self {
            path,
            value: Mutex::new(settings),
        };
        store.persist()?;
        Ok(store)
    }

    pub fn snapshot(&self) -> Settings {
        self.value.lock().clone()
    }

    pub fn replace(&self, mut settings: Settings) -> AppResult<Settings> {
        settings.settings_version = SETTINGS_VERSION;
        settings.validate()?;
        {
            let mut guard = self.value.lock();
            // 当前图库由后端维护；设置页提交不应覆盖成过期路径。
            settings.library_path = guard.library_path.clone();
            *guard = settings.clone();
        }
        self.persist()?;
        Ok(settings)
    }

    pub fn set_library_path(&self, path: Option<String>) -> AppResult<()> {
        let path = path.map(|value| {
            let mut trimmed = value.trim().to_owned();
            // 保留盘符根（G:\），其余尾部分隔符去掉，避免同一路径出现两种写法。
            while trimmed.len() > 3 && (trimmed.ends_with('\\') || trimmed.ends_with('/')) {
                trimmed.pop();
            }
            trimmed
        });
        {
            let mut guard = self.value.lock();
            guard.library_path = path;
            guard.validate()?;
        }
        self.persist()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&self) -> AppResult<()> {
        let settings = self.value.lock().clone();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temporary)?;
        serde_json::to_writer_pretty(&mut file, &settings).map_err(AppError::from)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

fn parse_settings(bytes: &[u8]) -> AppResult<Settings> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let version = value
        .get("settingsVersion")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1) as u32;
    if version < 2 {
        return migrate_v1(&value);
    }
    if version != SETTINGS_VERSION {
        return Err(AppError::message(format!(
            "设置文件版本 {version} 高于当前程序支持的 {SETTINGS_VERSION}"
        )));
    }
    Ok(serde_json::from_value(value)?)
}

/// v1 保存多个图库条目与 `lastLibraryId`；v2 只保留一个 `libraryPath`。
fn migrate_v1(value: &serde_json::Value) -> AppResult<Settings> {
    let libraries = value
        .get("libraries")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let last_id = value
        .get("lastLibraryId")
        .and_then(serde_json::Value::as_str);
    let chosen = last_id
        .and_then(|id| {
            libraries
                .iter()
                .find(|entry| entry.get("id").and_then(serde_json::Value::as_str) == Some(id))
        })
        .or_else(|| libraries.first());
    let library_path = chosen
        .and_then(|entry| entry.get("path"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty() && Path::new(path).is_absolute())
        .map(ToOwned::to_owned);

    let mut migrated = value.clone();
    if let Some(object) = migrated.as_object_mut() {
        object.insert(
            "settingsVersion".into(),
            serde_json::json!(SETTINGS_VERSION),
        );
        object.insert("libraryPath".into(), serde_json::json!(library_path));
        object.remove("libraries");
        object.remove("lastLibraryId");
    }
    Ok(serde_json::from_value(migrated)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        Settings {
            library_path: Some(r"G:\Lilith Image Library".into()),
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let store = SettingsStore::load_or_create(path.clone()).unwrap();
        store.replace(sample()).unwrap();
        store
            .set_library_path(Some(r"G:\Lilith Image Library\".into()))
            .unwrap();
        let loaded = SettingsStore::load_or_create(path).unwrap().snapshot();
        assert_eq!(loaded.settings_version, SETTINGS_VERSION);
        assert_eq!(
            loaded.library_path.as_deref(),
            Some(r"G:\Lilith Image Library")
        );
        assert_eq!(loaded.ui.theme, "light");
    }

    #[test]
    fn migrates_v1_prefers_last_library() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 1,
                "libraries": [
                    {"id": "a", "name": "参考", "path": "G:\\Lilith Image Library\\参考", "addedAt": 1},
                    {"id": "b", "name": "画作", "path": "G:\\Lilith Image Library\\画作", "addedAt": 2}
                ],
                "lastLibraryId": "b",
                "ai": {"baseUrl": "http://127.0.0.1:18766", "port": 18766, "autoStart": true},
                "performance": {"readerConnections": 4, "thumbnailCacheMb": 1024},
                "ui": {"theme": "light", "gridThumbPx": 220}
            }"#
            .as_bytes(),
        )
        .unwrap();
        let loaded = SettingsStore::load_or_create(path).unwrap().snapshot();
        assert_eq!(loaded.settings_version, SETTINGS_VERSION);
        assert_eq!(
            loaded.library_path.as_deref(),
            Some(r"G:\Lilith Image Library\画作")
        );
    }

    #[test]
    fn migrates_v1_without_last_library_to_first_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 1,
                "libraries": [
                    {"id": "a", "name": "参考", "path": "G:\\Lilith Image Library\\参考", "addedAt": 1}
                ],
                "lastLibraryId": null
            }"#
            .as_bytes(),
        )
        .unwrap();
        let loaded = SettingsStore::load_or_create(path).unwrap().snapshot();
        assert_eq!(
            loaded.library_path.as_deref(),
            Some(r"G:\Lilith Image Library\参考")
        );
    }

    #[test]
    fn keeps_backend_managed_library_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let store = SettingsStore::load_or_create(path).unwrap();
        store
            .set_library_path(Some(r"G:\Lilith Image Library".into()))
            .unwrap();
        let submitted = Settings {
            library_path: Some(r"D:\stale".into()),
            ..Default::default()
        };
        let saved = store.replace(submitted).unwrap();
        assert_eq!(
            saved.library_path.as_deref(),
            Some(r"G:\Lilith Image Library")
        );
    }

    #[test]
    fn rejects_relative_library_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settingsVersion": 2, "libraryPath": "relative"}"#,
        )
        .unwrap();
        assert!(SettingsStore::load_or_create(path).is_err());
    }
}

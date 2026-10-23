use std::{
    io::Write,
    path::{Path, PathBuf},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::app::error::{AppError, AppResult};

pub const SETTINGS_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub settings_version: u32,
    pub libraries: Vec<LibraryEntry>,
    pub last_library_id: Option<String>,
    pub ai: AiSettings,
    pub performance: PerformanceSettings,
    pub ui: UiSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            settings_version: SETTINGS_VERSION,
            libraries: Vec::new(),
            last_library_id: None,
            ai: AiSettings::default(),
            performance: PerformanceSettings::default(),
            ui: UiSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    pub id: String,
    pub name: String,
    pub path: String,
    pub added_at: i64,
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
        if self.libraries.len() > 64 {
            return Err(AppError::message("图库数量超过上限"));
        }
        let mut ids = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for library in &self.libraries {
            if library.id.trim().is_empty() {
                return Err(AppError::message("图库 ID 不能为空"));
            }
            let name = library.name.trim();
            if name.is_empty() || name.chars().count() > 120 {
                return Err(AppError::message("图库名称必须为 1-120 个字符"));
            }
            if !Path::new(&library.path).is_absolute() {
                return Err(AppError::message(format!(
                    "图库路径必须是绝对路径：{}",
                    library.path
                )));
            }
            if !ids.insert(library.id.clone()) {
                return Err(AppError::message("图库 ID 重复"));
            }
            if !paths.insert(normalize_path_key(&library.path)) {
                return Err(AppError::message("同一个图库路径被重复添加"));
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
                .and_then(|bytes| {
                    serde_json::from_slice::<Settings>(&bytes).map_err(AppError::from)
                }) {
                Ok(mut settings) => {
                    settings.settings_version = SETTINGS_VERSION;
                    settings
                }
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
            // 最近打开的图库由后端维护，设置页不应把它覆盖成过期值。
            settings.last_library_id = guard.last_library_id.clone();
            *guard = settings.clone();
        }
        self.persist()?;
        Ok(settings)
    }

    pub fn set_last_library(&self, library_id: Option<String>) -> AppResult<()> {
        {
            let mut guard = self.value.lock();
            guard.last_library_id = library_id;
        }
        self.persist()
    }

    pub fn upsert_library(&self, entry: LibraryEntry) -> AppResult<Settings> {
        {
            let mut guard = self.value.lock();
            if let Some(existing) = guard
                .libraries
                .iter_mut()
                .find(|item| same_path(&item.path, &entry.path) || item.id == entry.id)
            {
                existing.name = entry.name.clone();
                existing.path = entry.path.clone();
            } else {
                guard.libraries.push(entry);
            }
            guard.validate()?;
        }
        self.persist()?;
        Ok(self.snapshot())
    }

    pub fn remove_library(&self, library_id: &str) -> AppResult<Settings> {
        {
            let mut guard = self.value.lock();
            guard.libraries.retain(|item| item.id != library_id);
            if guard.last_library_id.as_deref() == Some(library_id) {
                guard.last_library_id = None;
            }
        }
        self.persist()?;
        Ok(self.snapshot())
    }

    pub fn rename_library(&self, library_id: &str, name: &str) -> AppResult<Settings> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(AppError::message("图库名称不能为空"));
        }
        {
            let mut guard = self.value.lock();
            let entry = guard
                .libraries
                .iter_mut()
                .find(|item| item.id == library_id)
                .ok_or_else(|| AppError::message("图库不在列表中"))?;
            entry.name = name;
            guard.validate()?;
        }
        self.persist()?;
        Ok(self.snapshot())
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

fn normalize_path_key(value: &str) -> String {
    value
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn same_path(left: &str, right: &str) -> bool {
    normalize_path_key(left) == normalize_path_key(right)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        let mut settings = Settings::default();
        settings.libraries.push(LibraryEntry {
            id: "lib-1".into(),
            name: "素材".into(),
            path: r"G:\Lilith Image Library\参考".into(),
            added_at: 1,
        });
        settings
    }

    #[test]
    fn round_trips_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let store = SettingsStore::load_or_create(path.clone()).unwrap();
        store.replace(sample()).unwrap();
        let loaded = SettingsStore::load_or_create(path).unwrap().snapshot();
        assert_eq!(loaded.libraries.len(), 1);
        assert_eq!(loaded.libraries[0].name, "素材");
        assert_eq!(loaded.ui.theme, "light");
    }

    #[test]
    fn rejects_duplicate_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let store = SettingsStore::load_or_create(path).unwrap();
        let mut settings = sample();
        settings.libraries.push(LibraryEntry {
            id: "lib-2".into(),
            name: "重复".into(),
            path: r"g:\lilith image library\参考".into(),
            added_at: 2,
        });
        assert!(store.replace(settings).is_err());
    }

    #[test]
    fn keeps_backend_managed_last_library() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let store = SettingsStore::load_or_create(path).unwrap();
        store.set_last_library(Some("lib-1".into())).unwrap();
        let mut submitted = sample();
        submitted.last_library_id = Some("stale".into());
        let saved = store.replace(submitted).unwrap();
        assert_eq!(saved.last_library_id.as_deref(), Some("lib-1"));
    }
}

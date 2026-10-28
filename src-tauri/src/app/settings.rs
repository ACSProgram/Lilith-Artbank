use std::{
    io::Write,
    path::{Path, PathBuf},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::app::error::{AppError, AppResult};
use crate::import::ImportOptimize;

pub const SETTINGS_VERSION: u32 = 7;
/// 操作记录保留步数范围；下限保证撤销至少能回到上一步，上限限制历史库体积。
pub const MIN_HISTORY_STEPS: u32 = 10;
pub const MAX_HISTORY_STEPS: u32 = 500;
pub const DEFAULT_HISTORY_STEPS: u32 = 100;
pub const DEFAULT_MCP_PORT: u16 = 18767;

const THEMES: [&str; 3] = ["light", "dark", "system"];
const THUMB_FITS: [&str; 2] = ["cover", "contain"];
const SORT_KEYS: [&str; 7] = [
    "importedAt",
    "modifiedAt",
    "score",
    "size",
    "pixels",
    "folder",
    "collectionPosition",
];

/// 排序级数上限：与查询层的指标数量一致，不额外设人为限制（每个指标最多用一次）。
pub const MAX_SORT_LEVELS: usize = crate::library::query::MAX_SORT_LEVELS;
/// 排序方案是存储上限，与排序级数无关。
pub const MAX_SORT_PROFILES: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub settings_version: u32,
    pub library_path: Option<String>,
    pub ai: AiSettings,
    pub performance: PerformanceSettings,
    pub history: HistorySettings,
    pub import: ImportSettings,
    pub mcp: McpSettings,
    pub ui: UiSettings,
    pub maintenance: MaintenanceSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            settings_version: SETTINGS_VERSION,
            library_path: None,
            ai: AiSettings::default(),
            performance: PerformanceSettings::default(),
            history: HistorySettings::default(),
            import: ImportSettings::default(),
            mcp: McpSettings::default(),
            ui: UiSettings::default(),
            maintenance: MaintenanceSettings::default(),
        }
    }
}

/// 导入偏好：目前只记住「导入时压缩」的开关与参数。
///
/// 目标文件夹、目录层级、重复策略、评分与标签都是每次导入现选的，不进设置；
/// 组里只留一个字段，位置留给以后真正需要持久化的导入偏好。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImportSettings {
    /// `None` 表示关闭（默认）；打开时记住档位与输出格式。
    pub optimize: Option<ImportOptimize>,
}

/// MCP 服务默认关闭，开启后使用询问档；批次 1 尚未注册写工具，实际能力为只读。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpMode {
    ReadOnly,
    #[default]
    Ask,
    Full,
}

impl McpMode {
    /// 与 serde 的 camelCase 输出一致的字符串，供工具返回值复用。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "readOnly",
            Self::Ask => "ask",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpSettings {
    pub enabled: bool,
    pub port: u16,
    pub mode: McpMode,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_MCP_PORT,
            mode: McpMode::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HistorySettings {
    /// 每个图库保留的操作记录步数；降低上限会立即裁剪并释放旧文件。
    pub max_steps: u32,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_HISTORY_STEPS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AiSettings {
    pub base_url: String,
    pub port: u16,
    pub auto_start: bool,
    /// 为空时只连接 `base_url` 上已经启动的服务。
    pub python_path: Option<String>,
    pub server_script: Option<String>,
    /// 模型根目录。
    pub model_path: Option<String>,
    /// CLIP 模型目录；相对 `model_path`，也可以填绝对路径。
    pub clip_model_path: Option<String>,
    /// 服务启动后在后台导入 torch/transformers；关掉可省下约 420 MB 常驻内存。
    pub prewarm_imports: bool,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:18766".into(),
            port: 18766,
            auto_start: true,
            python_path: Some(r"python_embeded\python.exe".into()),
            server_script: Some(r"server\main.py".into()),
            model_path: Some(r"resources\models".into()),
            clip_model_path: Some("siglip2-giant-opt-patch16-384".into()),
            prewarm_imports: true,
        }
    }
}

impl AiSettings {
    pub fn python_path(&self) -> Option<&str> {
        self.python_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    pub fn server_script(&self) -> Option<&str> {
        self.server_script
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    pub fn launch_configured(&self) -> bool {
        self.base_url_is_local() && self.python_path().is_some() && self.server_script().is_some()
    }

    /// 只识别本机 HTTP 地址；`https://` 与外部主机都算外部服务。
    pub fn base_url_is_local(&self) -> bool {
        local_authority(self.base_url.trim()).is_some()
    }
}

/// 拆出本机 HTTP 地址的端口；不是本机地址或没有端口时返回 `None`。
fn local_base_port(url: &str) -> Option<u16> {
    local_authority(url).and_then(|(_, port)| port)
}

/// 返回 `(host, port)`；不是本机 HTTP 地址时返回 `None`。
///
/// 主机必须是 `127.0.0.1`、`localhost` 或 `[::1]`，`127.0.0.10`、
/// `localhost.example.com` 这类前缀相同的外部主机不算本机。
fn local_authority(url: &str) -> Option<(&str, Option<u16>)> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse().ok()?)),
        None => (authority, None),
    };
    let is_local = host.eq_ignore_ascii_case("127.0.0.1")
        || host.eq_ignore_ascii_case("localhost")
        || host == "[::1]";
    is_local.then_some((host, port))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PerformanceSettings {
    /// 元数据读取连接数。写连接始终只有一条，避免 SQLite 写放大。
    pub reader_connections: u32,
    /// 为空时使用 CPU 可用并行度减去一个线程。
    pub worker_threads: Option<usize>,
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        Self {
            reader_connections: 4,
            worker_threads: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SortSpec {
    pub key: String,
    pub descending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: "importedAt".into(),
            descending: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SortProfile {
    pub id: String,
    pub name: String,
    pub sort: Vec<SortSpec>,
}

impl Default for SortProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            sort: vec![SortSpec::default()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiSettings {
    pub theme: String,
    pub grid_thumb_px: u32,
    pub thumb_fit: String,
    /// 缩略图缓存按张数，不再用 MB。
    pub thumb_cache_count: u32,
    /// 全局一套排序，叠加到全部图库视图。
    pub sort: Vec<SortSpec>,
    pub sort_profiles: Vec<SortProfile>,
    pub viewer_filmstrip: bool,
    /// 列表视图显示哪些列；缺键或为空时按默认全显示。
    pub list_columns: Vec<String>,
}

/// 列表视图可隐藏的列；缩略图列不在其中，永远显示。
pub const LIST_COLUMNS: [&str; 7] = [
    "pixels",
    "format",
    "size",
    "score",
    "tags",
    "importedAt",
    "folder",
];

pub fn default_list_columns() -> Vec<String> {
    LIST_COLUMNS
        .iter()
        .map(|column| (*column).to_owned())
        .collect()
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            grid_thumb_px: 220,
            thumb_fit: "cover".into(),
            thumb_cache_count: 1024,
            sort: vec![SortSpec::default()],
            sort_profiles: Vec::new(),
            viewer_filmstrip: true,
            list_columns: default_list_columns(),
        }
    }
}

/// 维护相关开关；缺键走默认值，不升 `SETTINGS_VERSION`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MaintenanceSettings {
    /// 空闲时自动回收缩略图包；关掉之后「立即回收」仍然可用。
    pub auto_compact_thumbs: bool,
}

impl Default for MaintenanceSettings {
    fn default() -> Self {
        Self {
            auto_compact_thumbs: true,
        }
    }
}

fn validate_sort(sort: &[SortSpec]) -> AppResult<()> {
    if sort.is_empty() {
        return Err(AppError::message("排序至少要有一级"));
    }
    if sort.len() > MAX_SORT_LEVELS {
        return Err(AppError::message(format!("排序最多 {MAX_SORT_LEVELS} 级")));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(sort.len());
    for spec in sort {
        if !SORT_KEYS.contains(&spec.key.as_str()) {
            return Err(AppError::message(format!("未知的排序指标：{}", spec.key)));
        }
        if seen.contains(&spec.key.as_str()) {
            return Err(AppError::message(format!("排序指标重复：{}", spec.key)));
        }
        seen.push(&spec.key);
    }
    Ok(())
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
        if self.ai.python_path().is_some() && self.ai.server_script().is_none() {
            return Err(AppError::message("填写 Python 路径时也要填写服务脚本"));
        }
        if let Some(url_port) = local_base_port(url)
            && url_port != self.ai.port
        {
            return Err(AppError::message(
                "AI 服务地址端口与启动端口不一致，请改成同一个端口",
            ));
        }
        if !(1..=16).contains(&self.performance.reader_connections) {
            return Err(AppError::message("读取连接数必须在 1-16 之间"));
        }
        if let Some(threads) = self.performance.worker_threads
            && !(1..=128).contains(&threads)
        {
            return Err(AppError::message("后台图像工作线程数必须在 1-128 之间"));
        }
        if !(MIN_HISTORY_STEPS..=MAX_HISTORY_STEPS).contains(&self.history.max_steps) {
            return Err(AppError::message(format!(
                "操作记录步数必须在 {MIN_HISTORY_STEPS}-{MAX_HISTORY_STEPS} 之间"
            )));
        }
        if self.mcp.port == 0 {
            return Err(AppError::message("MCP 端口必须在 1-65535 之间"));
        }
        if !THEMES.contains(&self.ui.theme.as_str()) {
            return Err(AppError::message("主题只能是 light、dark 或 system"));
        }
        if !(96..=512).contains(&self.ui.grid_thumb_px) {
            return Err(AppError::message("网格缩略图尺寸必须在 96-512 像素之间"));
        }
        if !THUMB_FITS.contains(&self.ui.thumb_fit.as_str()) {
            return Err(AppError::message("缩略图填充方式只能是 cover 或 contain"));
        }
        if !(256..=8192).contains(&self.ui.thumb_cache_count) {
            return Err(AppError::message("缩略图缓存张数必须在 256-8192 之间"));
        }
        validate_sort(&self.ui.sort)?;
        if self
            .ui
            .list_columns
            .iter()
            .all(|column| !LIST_COLUMNS.contains(&column.as_str()))
        {
            return Err(AppError::message("列表至少要保留一列"));
        }
        if self.ui.sort_profiles.len() > MAX_SORT_PROFILES {
            return Err(AppError::message(format!(
                "排序方案最多保存 {MAX_SORT_PROFILES} 个"
            )));
        }
        let mut names: Vec<String> = Vec::with_capacity(self.ui.sort_profiles.len());
        for profile in &self.ui.sort_profiles {
            let name = profile.name.trim();
            if name.is_empty() {
                return Err(AppError::message("排序方案名称不能为空"));
            }
            let folded = name.to_lowercase();
            if names.contains(&folded) {
                return Err(AppError::message(format!("排序方案名称重复：{name}")));
            }
            names.push(folded);
            // 方案内的排序走同一套校验，报错时带上方案名，否则定位不到是哪一个。
            validate_sort(&profile.sort)
                .map_err(|error| AppError::message(format!("排序方案「{name}」：{error}")))?;
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
            // 导入压缩由导入对话框的窄命令维护，设置页提交整份 Settings 时保持原值，
            // 否则保存一次主题就会把这里的开关抹掉。
            settings.import = guard.import.clone();
            *guard = settings.clone();
        }
        self.persist()?;
        Ok(settings)
    }

    /// 只改「导入时压缩」这一个字段并落盘；返回落盘后的值。
    ///
    /// 不走 `replace`：设置页提交的是整份 `Settings`，两条写路径互相覆盖会丢用户刚选的档位。
    pub fn set_import_optimize(
        &self,
        optimize: Option<ImportOptimize>,
    ) -> AppResult<Option<ImportOptimize>> {
        {
            let mut guard = self.value.lock();
            guard.import.optimize = optimize;
        }
        self.persist()?;
        Ok(self.value.lock().import.optimize)
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
    let mut value: serde_json::Value = serde_json::from_slice(bytes)?;
    let version = value
        .get("settingsVersion")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1) as u32;
    if version > SETTINGS_VERSION {
        return Err(AppError::message(format!(
            "设置文件版本 {version} 高于当前程序支持的 {SETTINGS_VERSION}"
        )));
    }
    if version < 2 {
        value = migrate_v1(&value)?;
    }
    if version < 3 {
        value = migrate_v2(value);
    }
    if version < 4 {
        value = migrate_v3(value);
    }
    if version < 5 {
        value = migrate_v4(value);
    }
    if version < 6 {
        value = migrate_v5(value);
    }
    if version < 7 {
        value = migrate_v6(value);
    }
    Ok(serde_json::from_value(value)?)
}

/// v1 保存多个图库条目与 `lastLibraryId`；v2 只保留一个 `libraryPath`。
fn migrate_v1(value: &serde_json::Value) -> AppResult<serde_json::Value> {
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
        object.insert("settingsVersion".into(), serde_json::json!(2));
        object.insert("libraryPath".into(), serde_json::json!(library_path));
        object.remove("libraries");
        object.remove("lastLibraryId");
    }
    Ok(migrated)
}

/// v2 → v3：删掉从未生效的 `performance.thumbnailCacheMb`，新字段由 serde 默认值补齐。
fn migrate_v2(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("settingsVersion".into(), serde_json::json!(3));
        if let Some(performance) = object
            .get_mut("performance")
            .and_then(serde_json::Value::as_object_mut)
        {
            performance.remove("thumbnailCacheMb");
        }
    }
    value
}

/// v3 → v4：`ai.executablePath` 换成 Python 启动配置；旧字段直接丢弃。
fn migrate_v3(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("settingsVersion".into(), serde_json::json!(4));
        if let Some(ai) = object
            .get_mut("ai")
            .and_then(serde_json::Value::as_object_mut)
        {
            ai.remove("executablePath");
        }
    }
    value
}

/// v4 → v5：补操作记录最大步数；缺失时用默认值，旧字段全部保留。
fn migrate_v4(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("settingsVersion".into(), serde_json::json!(5));
        object
            .entry("history")
            .or_insert_with(|| serde_json::json!({ "maxSteps": DEFAULT_HISTORY_STEPS }));
    }
    value
}

/// v5 → v6：新增 MCP 服务配置；默认关闭，开启后端口 18767、权限档为询问。
fn migrate_v5(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "settingsVersion".into(),
            serde_json::json!(SETTINGS_VERSION),
        );
        object.entry("mcp").or_insert_with(|| {
            serde_json::to_value(McpSettings::default()).unwrap_or_else(|_| {
                serde_json::json!({
                    "enabled": false,
                    "port": DEFAULT_MCP_PORT,
                    "mode": "ask",
                })
            })
        });
    }
    value
}

/// v6 → v7：新增 `ai.prewarmImports`（默认开）。字段本身由 `serde(default)` 补齐，
/// 这里显式写一遍是为了让“默认开”这件事留在迁移链上。
fn migrate_v6(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "settingsVersion".into(),
            serde_json::json!(SETTINGS_VERSION),
        );
        if let Some(ai) = object.get_mut("ai").and_then(|ai| ai.as_object_mut()) {
            ai.entry("prewarmImports")
                .or_insert_with(|| serde_json::json!(true));
        }
    }
    value
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

    /// 旧 `settings.json` 缺 `listColumns` / `maintenance` 时走默认值，不升版本、不写迁移。
    #[test]
    fn old_settings_without_new_keys_use_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(&path, r#"{"settingsVersion":7,"ui":{"theme":"dark"}}"#).unwrap();
        let store = SettingsStore::load_or_create(path).unwrap();
        let settings = store.snapshot();
        assert_eq!(settings.ui.theme, "dark");
        assert_eq!(settings.ui.list_columns, default_list_columns());
        assert_eq!(settings.ui.list_columns.len(), LIST_COLUMNS.len());
        assert!(settings.maintenance.auto_compact_thumbs, "默认开启自动回收");
    }

    #[test]
    fn list_columns_must_keep_one() {
        let mut settings = sample();
        settings.ui.list_columns = vec!["unknown".into()];
        assert!(settings.validate().is_err());
        settings.ui.list_columns = vec!["folder".into()];
        assert!(settings.validate().is_ok());
    }

    /// 新增一个 `SortKey` 时必须同步这里的字符串列表，否则设置里的排序会被判成未知指标。
    #[test]
    fn sort_key_list_matches_query_limit() {
        assert_eq!(SORT_KEYS.len(), MAX_SORT_LEVELS);
    }

    /// 导入压缩默认关闭；存下来后重启仍在，取消勾选后回到 `None`。
    #[test]
    fn import_optimize_persists_and_defaults_off() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let store = SettingsStore::load_or_create(path.clone()).unwrap();
        assert_eq!(store.snapshot().import, ImportSettings::default());
        assert!(store.snapshot().import.optimize.is_none(), "默认关闭");

        let option = ImportOptimize {
            tier: crate::imageops::optimize::QualityTier::Medium,
            output_format: crate::imageops::optimize::OptimizeFormat::WebP,
        };
        assert_eq!(
            store.set_import_optimize(Some(option)).unwrap(),
            Some(option)
        );
        let loaded = SettingsStore::load_or_create(path.clone())
            .unwrap()
            .snapshot();
        assert_eq!(loaded.import.optimize, Some(option));
        // 设置页保存整份 Settings 时不能把这个开关抹掉。
        let replaced = store.replace(sample()).unwrap();
        assert_eq!(replaced.import.optimize, Some(option));
        assert_eq!(
            SettingsStore::load_or_create(path.clone())
                .unwrap()
                .snapshot()
                .import
                .optimize,
            Some(option)
        );

        assert_eq!(store.set_import_optimize(None).unwrap(), None);
        let loaded = SettingsStore::load_or_create(path).unwrap().snapshot();
        assert!(loaded.import.optimize.is_none());
    }

    /// 旧文件里没有 `import` 键时按默认（关闭）读进来，不需要升版本号。
    #[test]
    fn missing_import_group_falls_back_to_off() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        value.as_object_mut().unwrap().remove("import");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let loaded = SettingsStore::load_or_create(path).unwrap().snapshot();
        assert!(loaded.import.optimize.is_none());
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
        assert_eq!(loaded.ui.theme, "system");
        assert_eq!(loaded.ui.thumb_fit, "cover");
        assert_eq!(loaded.ui.thumb_cache_count, 1024);
        assert_eq!(loaded.history.max_steps, DEFAULT_HISTORY_STEPS);
        assert_eq!(loaded.mcp, McpSettings::default());
        assert!(!loaded.mcp.enabled);
        assert_eq!(loaded.mcp.port, DEFAULT_MCP_PORT);
        assert_eq!(loaded.mcp.mode, McpMode::Ask);
        assert_eq!(loaded.ui.sort, vec![SortSpec::default()]);
        assert_eq!(
            loaded.ai.python_path.as_deref(),
            Some(r"python_embeded\python.exe")
        );
        assert_eq!(loaded.ai.server_script.as_deref(), Some(r"server\main.py"));
        assert_eq!(loaded.ai.model_path.as_deref(), Some(r"resources\models"));
        assert_eq!(
            loaded.ai.clip_model_path.as_deref(),
            Some("siglip2-giant-opt-patch16-384")
        );
        assert!(loaded.ai.launch_configured());
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
        assert_eq!(loaded.ui.theme, "light");
        assert_eq!(loaded.ui.grid_thumb_px, 220);
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
    fn migrates_v2_to_current_and_drops_dead_cache_field() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 2,
                "libraryPath": "G:\\Lilith Image Library",
                "ai": {"baseUrl": "http://127.0.0.1:18766", "port": 18766, "autoStart": false},
                "performance": {"readerConnections": 6, "workerThreads": 3, "thumbnailCacheMb": 2048},
                "ui": {"theme": "dark", "gridThumbPx": 264}
            }"#
            .as_bytes(),
        )
        .unwrap();
        let loaded = SettingsStore::load_or_create(path.clone())
            .unwrap()
            .snapshot();
        assert_eq!(loaded.settings_version, SETTINGS_VERSION);
        assert_eq!(loaded.ui.theme, "dark");
        assert_eq!(loaded.ui.grid_thumb_px, 264);
        assert_eq!(loaded.performance.reader_connections, 6);
        assert_eq!(loaded.performance.worker_threads, Some(3));
        assert!(!loaded.ai.auto_start);
        // 默认值补齐的新字段
        assert_eq!(loaded.ui.thumb_fit, "cover");
        assert_eq!(loaded.ui.thumb_cache_count, 1024);
        assert!(loaded.ui.viewer_filmstrip);

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("thumbnailCacheMb"));
        assert!(written.contains("\"settingsVersion\": 7"));
    }

    #[test]
    fn migrates_v3_to_current_and_replaces_executable_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 3,
                "libraryPath": "G:\\Lilith Image Library",
                "ai": {
                    "baseUrl": "http://127.0.0.1:19000",
                    "port": 19000,
                    "autoStart": true,
                    "executablePath": "C:\\Lilith Server\\server.exe"
                }
            }"#
            .as_bytes(),
        )
        .unwrap();
        let loaded = SettingsStore::load_or_create(path.clone())
            .unwrap()
            .snapshot();
        assert_eq!(loaded.settings_version, SETTINGS_VERSION);
        assert_eq!(loaded.ai.port, 19000);
        assert!(loaded.ai.auto_start);
        assert_eq!(
            loaded.ai.python_path.as_deref(),
            Some(r"python_embeded\python.exe")
        );
        assert_eq!(
            loaded.ai.clip_model_path.as_deref(),
            Some("siglip2-giant-opt-patch16-384")
        );

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("executablePath"));
        assert!(written.contains("\"settingsVersion\": 7"));
        assert!(written.contains("\"maxSteps\": 100"));
    }

    #[test]
    fn migrates_v4_to_current_and_adds_history_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 4,
                "libraryPath": "G:\\Lilith Image Library",
                "ui": {"theme": "dark", "gridThumbPx": 300},
                "performance": {"readerConnections": 6}
            }"#
            .as_bytes(),
        )
        .unwrap();
        let loaded = SettingsStore::load_or_create(path.clone())
            .unwrap()
            .snapshot();
        assert_eq!(loaded.settings_version, SETTINGS_VERSION);
        assert_eq!(loaded.history.max_steps, DEFAULT_HISTORY_STEPS);
        assert_eq!(loaded.ui.theme, "dark");
        assert_eq!(loaded.ui.grid_thumb_px, 300);
        assert_eq!(loaded.performance.reader_connections, 6);
        assert_eq!(loaded.mcp, McpSettings::default());

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("\"settingsVersion\": 7"));
        assert!(written.contains("\"maxSteps\": 100"));
    }

    #[test]
    fn migrates_v5_to_current_and_adds_mcp_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 5,
                "libraryPath": "G:\\Lilith Image Library",
                "history": {"maxSteps": 250}
            }"#
            .as_bytes(),
        )
        .unwrap();
        let loaded = SettingsStore::load_or_create(path.clone())
            .unwrap()
            .snapshot();
        assert_eq!(loaded.settings_version, SETTINGS_VERSION);
        assert_eq!(loaded.history.max_steps, 250);
        assert_eq!(loaded.mcp, McpSettings::default());

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("\"settingsVersion\": 7"));
        assert!(written.contains("\"mcp\""));
        assert!(written.contains("\"enabled\": false"));
        assert!(written.contains("\"port\": 18767"));
        assert!(written.contains("\"mode\": \"ask\""));
    }

    #[test]
    fn migrates_v6_to_current_and_enables_import_prewarm() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 6,
                "libraryPath": "G:\\Lilith Image Library",
                "ai": {
                    "baseUrl": "http://127.0.0.1:18766",
                    "port": 18766,
                    "autoStart": false,
                    "clipModelPath": "siglip2-test"
                }
            }"#
            .as_bytes(),
        )
        .unwrap();
        let loaded = SettingsStore::load_or_create(path.clone())
            .unwrap()
            .snapshot();
        assert_eq!(loaded.settings_version, SETTINGS_VERSION);
        assert!(!loaded.ai.auto_start, "已有字段保持不变");
        assert_eq!(loaded.ai.clip_model_path.as_deref(), Some("siglip2-test"));
        assert!(loaded.ai.prewarm_imports, "v6 升上来默认开启预热");

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("\"prewarmImports\": true"));
    }

    #[test]
    fn rejects_invalid_mcp_port() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settingsVersion": 6, "mcp": {"enabled": true, "port": 0}}"#,
        )
        .unwrap();
        let error = SettingsStore::load_or_create(path).err().unwrap();
        assert!(error.to_string().contains("MCP 端口"));
    }

    #[test]
    fn mcp_mode_strings_match_serialization() {
        for mode in [McpMode::ReadOnly, McpMode::Ask, McpMode::Full] {
            let serialized = serde_json::to_value(mode).unwrap();
            assert_eq!(serialized, serde_json::json!(mode.as_str()));
        }
    }

    #[test]
    fn rejects_history_steps_out_of_range() {
        for value in [0, MIN_HISTORY_STEPS - 1, MAX_HISTORY_STEPS + 1] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("settings.json");
            std::fs::write(
                &path,
                format!(r#"{{"settingsVersion": 5, "history": {{"maxSteps": {value}}}}}"#),
            )
            .unwrap();
            let error = SettingsStore::load_or_create(path).err().unwrap();
            assert!(
                error.to_string().contains("操作记录步数"),
                "{value} 应被拒绝"
            );
        }
    }

    #[test]
    fn rejects_python_without_server_script() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 4,
                "ai": {
                    "pythonPath": "python_embeded\\python.exe",
                    "serverScript": ""
                }
            }"#,
        )
        .unwrap();
        let error = SettingsStore::load_or_create(path).err().unwrap();
        assert!(error.to_string().contains("服务脚本"));
    }

    #[test]
    fn rejects_local_base_url_port_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "settingsVersion": 4,
                "ai": {
                    "baseUrl": "http://127.0.0.1:19000",
                    "port": 18766
                }
            }"#,
        )
        .unwrap();
        let error = SettingsStore::load_or_create(path).err().unwrap();
        assert!(error.to_string().contains("端口"));
    }

    #[test]
    fn recognises_only_real_local_base_urls() {
        let local = |url: &str| AiSettings {
            base_url: url.to_owned(),
            ..Default::default()
        };
        for url in [
            "http://127.0.0.1:18766",
            "http://127.0.0.1",
            "http://localhost:18766/v1",
            "http://LOCALHOST:18766",
            "http://[::1]:18766",
        ] {
            assert!(local(url).base_url_is_local(), "{url} 应当是本机地址");
        }
        for url in [
            "https://127.0.0.1:18766",
            "http://127.0.0.10:18766",
            "http://localhost.example.com:18766",
            "http://192.168.1.5:18766",
            "http://127.0.0.1:not-a-port",
        ] {
            assert!(!local(url).base_url_is_local(), "{url} 不应当是本机地址");
        }
    }

    #[test]
    fn reads_port_only_from_local_http_urls() {
        assert_eq!(local_base_port("http://127.0.0.1:19000"), Some(19000));
        assert_eq!(local_base_port("http://localhost:19000/v1"), Some(19000));
        assert_eq!(local_base_port("http://127.0.0.1"), None);
        assert_eq!(local_base_port("http://192.168.1.5:19000"), None);
    }

    #[test]
    fn rejects_unknown_theme() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(&path, r#"{"settingsVersion": 3, "ui": {"theme": "sepia"}}"#).unwrap();
        let error = SettingsStore::load_or_create(path).err().unwrap();
        assert!(error.to_string().contains("主题"));
    }

    #[test]
    fn rejects_duplicate_sort_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"settingsVersion": 3, "ui": {"sort": [{"key": "score", "descending": true}, {"key": "score", "descending": false}]}}"#,
        )
        .unwrap();
        let error = SettingsStore::load_or_create(path).err().unwrap();
        assert!(error.to_string().contains("重复"));
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

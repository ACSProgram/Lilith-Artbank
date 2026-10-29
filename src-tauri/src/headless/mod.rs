//! 无窗口验收入口（`feature = "headless"`）。
//!
//! 只服务于开发期验收：不创建用户可见窗口、不加载 WebView、不经过 IPC。入口是命令层
//! 的第二适配器——解析参数、取当前图库、调用与 Tauri 命令相同的 `AppState`/领域函数，
//! 输出一行 JSON。业务判断不写在这里。
//!
//! 约定：`--library` 在执行命令前显式打开图库（无头模式不自动打开上次图库）；
//! `--request`/`--request-json` 传 DTO；`--result` 另存一份格式化结果。退出码：
//! 0 成功、1 失败、2 取消。

mod ai;
mod maintenance;
mod mcp;
mod optimize;
mod write;

use std::{
    cell::Cell,
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tauri::Manager;

use crate::{
    app::{
        self,
        error::AppError,
        settings::{Settings, SettingsStore},
        state::AppState,
        tasks::{TaskPhase, TaskSnapshot},
        trace,
    },
    library::{
        BatchReporter,
        fixture::{self, SeedSpec},
        manifest::LibraryInspection,
        query::LibraryQuery,
        service::LibraryInfo,
        stats::StatsRequest,
    },
};

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_CANCELLED: i32 = 2;

pub fn run(args: &[String]) -> i32 {
    let started = Instant::now();
    if args.is_empty() || matches!(args[0].as_str(), "help" | "--help" | "-h") {
        eprint!("{}", usage());
        return EXIT_OK;
    }
    let command = args[0].clone();
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("lilith-artbank --headless: {error}");
            eprint!("{}", usage());
            return EXIT_ERROR;
        }
    };

    let outcome = run_command(&options);
    let (ok, cancelled, error, data) = match outcome {
        Ok((data, cancelled)) => (true, cancelled, None, data),
        Err(error) => (false, false, Some(error), Value::Null),
    };
    let result = HeadlessResult {
        command,
        ok,
        cancelled,
        error,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        peak_working_set_bytes: peak_working_set_bytes(),
        data,
    };
    if let Some(path) = options.result.as_deref()
        && let Err(error) = write_json(path, &result)
    {
        eprintln!("lilith-artbank --headless: {error}");
    }
    match serde_json::to_string(&result) {
        Ok(line) => {
            println!("{line}");
            let _ = std::io::stdout().flush();
        }
        Err(error) => eprintln!("lilith-artbank --headless: 无法序列化结果：{error}"),
    }

    if result.cancelled {
        EXIT_CANCELLED
    } else if result.ok {
        EXIT_OK
    } else {
        EXIT_ERROR
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HeadlessResult {
    command: String,
    ok: bool,
    cancelled: bool,
    error: Option<String>,
    elapsed_ms: u64,
    peak_working_set_bytes: Option<u64>,
    data: Value,
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("无法序列化结果：{error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建结果目录：{error}"))?;
    }
    fs::write(path, bytes).map_err(|error| format!("无法写入结果文件：{error}"))
}

fn run_command(options: &Options) -> Result<(Value, bool), String> {
    let root = require_root()?;
    let settings_path = app::paths::settings_path();
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建设置目录 {}：{error}", parent.display()))?;
    }
    let settings = SettingsStore::load_or_create(settings_path.clone())
        .map_err(|error| format!("无法加载设置：{error}"))?;

    let log_dir = app::paths::log_dir();
    let app = tauri::Builder::default()
        .plugin(app::logging::plugin(log_dir, false))
        .build(tauri::generate_context!())
        .map_err(|error| format!("无法创建无窗口应用：{error}"))?;
    // 插件在 setup 里安装 logger 并自行设置一次 max_level；这里必须在其后覆盖。
    log::set_max_level(app::trace::baseline_level());
    app::logging::startup_banner(&root);

    let state = AppState::new(settings, app.handle().clone());
    let _ = app.manage(state);
    app.state::<AppState>().after_manage();

    let cancelled = Cell::new(false);
    let outcome = (|| {
        if let Some(path) = options.library.as_deref() {
            open_library(app.handle(), path)?;
        }
        dispatch(app.handle(), options, &root, &cancelled)
    })();

    {
        let state = app.state::<AppState>();
        state.shutdown();
        let ai = state.settings.snapshot().ai;
        state.ai.shutdown(&ai);
        state.mcp.stop();
    }
    drop(app);
    outcome.map(|data| (data, cancelled.get()))
}

/// 批量任务在命令里同步等到终态，超时即报错而不是一直挂着。
const TASK_TIMEOUT: Duration = Duration::from_secs(120);

/// 领域批量写函数只关心取消标志，命令行不需要逐张进度。
pub(super) struct NullReporter;

impl BatchReporter for NullReporter {
    fn report(&self, _completed: usize, _total: usize) {}

    fn cancelled(&self) -> bool {
        false
    }
}

/// 用默认上限等任务结束。
pub(super) fn wait_task(state: &AppState, task_id: &str) -> Result<TaskSnapshot, String> {
    wait_task_with_timeout(state, task_id, TASK_TIMEOUT)
}

/// 轮询任务快照，直到进入终态或超时。
///
/// 任务结束后会从总线里摘掉，此时只剩「超时」一条路；调用方据此报错即可。
pub(super) fn wait_task_with_timeout(
    state: &AppState,
    task_id: &str,
    timeout: Duration,
) -> Result<TaskSnapshot, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(snapshot) = state
            .tasks_snapshot()
            .into_iter()
            .find(|snapshot| snapshot.task_id == task_id)
            && snapshot.phase.is_terminal()
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(format!("等待任务 {task_id} 结束超时"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 失败翻成命令错误；取消置位取消标志，让入口以退出码 2 收尾。
pub(super) fn check_task(snapshot: &TaskSnapshot, cancelled: &Cell<bool>) -> Result<(), String> {
    match snapshot.phase {
        TaskPhase::Failed => Err(if snapshot.message.is_empty() {
            "任务失败".to_owned()
        } else {
            snapshot.message.clone()
        }),
        TaskPhase::Cancelled => {
            cancelled.set(true);
            Ok(())
        }
        _ => Ok(()),
    }
}

fn dispatch(
    app: &tauri::AppHandle,
    options: &Options,
    root: &Path,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let state = state.inner();
    match (options.command.as_str(), options.action.as_deref()) {
        ("ping", None) => {
            let library = state.status().map_err(|error| error.to_string())?;
            Ok(json!({
                "appRoot": root.display().to_string(),
                "settingsPath": app::paths::settings_path().display().to_string(),
                "logDir": app::paths::log_dir().display().to_string(),
                "library": library,
                "pid": std::process::id(),
            }))
        }
        ("library", Some("create")) => {
            let request: LibraryCreate = options.request()?;
            value_of(trace::write("create_library", || {
                AppError::command(state.create_library(Path::new(&request.path), &request.name))
            }))
        }
        ("library", Some("open")) => {
            let request: LibraryPath = options.request()?;
            value_of(open_library(app, Path::new(&request.path)))
        }
        ("library", Some("open-configured")) => {
            let configured = state.settings.snapshot().library_path;
            match configured {
                None => Ok(json!({ "configured": false, "library": Value::Null })),
                Some(path) => {
                    let info = open_library(app, Path::new(&path))?;
                    Ok(json!({ "configured": true, "library": info }))
                }
            }
        }
        ("library", Some("close")) => {
            trace::write("close_library", || AppError::command(state.close_library()))?;
            Ok(json!({ "closed": true }))
        }
        ("library", Some("info")) => value_of(trace::read("get_library_status", || {
            state.status().map_err(|error| error.to_string())
        })),
        ("library", Some("counts")) => value_of(trace::read("get_library_counts", || {
            AppError::command(state.counts())
        })),
        ("library", Some("inspect-path")) => {
            let request: LibraryPath = options.request()?;
            value_of(trace::read("inspect_library_path", || {
                Ok(state.inspect_library(Path::new(&request.path)))
            }))
        }
        ("settings", Some("get")) => value_of(Ok(state.settings.snapshot())),
        ("settings", Some("save")) => {
            let settings: Settings = options.request()?;
            value_of(trace::write("save_settings", || {
                save_settings(state, settings)
            }))
        }
        ("query", Some("images")) => {
            let query: LibraryQuery = options.request_or_default()?;
            value_of(trace::read("query_images", || {
                AppError::command(state.query_images(query))
            }))
        }
        ("query", Some("ids")) => {
            let query: LibraryQuery = options.request_or_default()?;
            value_of(trace::read("query_image_ids", || {
                AppError::command(state.query_image_ids(query))
            }))
        }
        ("query", Some("stats")) => {
            let request: StatsRequest = options.request()?;
            value_of(trace::read("get_library_stats", || {
                AppError::command(state.library_stats(request))
            }))
        }
        ("query", Some("folders")) => value_of(trace::read("get_folders", || {
            AppError::command(state.folders())
        })),
        ("query", Some("tags")) => {
            let request: TagsRequest = options.request_or_default()?;
            value_of(trace::read("get_tags", || {
                let result = if request.search.trim().is_empty() {
                    state.tags(request.limit, request.include_empty)
                } else {
                    state.search_tags(&request.search, request.limit, request.include_empty)
                };
                AppError::command(result)
            }))
        }
        ("query", Some("detail")) => {
            let request: ImageIdRequest = options.request()?;
            value_of(trace::read("get_image_detail", || {
                AppError::command(state.image_detail(request.image_id))
            }))
        }
        ("query", Some("states")) => {
            let request: ImageIdsRequest = options.request_or_default()?;
            value_of(trace::read("get_image_states", || {
                AppError::command(state.image_states(&request.image_ids))
            }))
        }
        ("query", Some("selection-tags")) => {
            let request: ImageIdsRequest = options.request_or_default()?;
            value_of(trace::read("get_selection_tags", || {
                AppError::command(state.selection_tags(&request.image_ids))
            }))
        }
        ("fixture", Some("seed-db")) => {
            let spec: SeedSpec = options.request_or_default()?;
            let (database, root) = state
                .current_library()
                .ok_or_else(|| "尚未打开图库".to_owned())?;
            let report = database
                .with_writer(|connection| fixture::seed(connection, &spec, &root))
                .map_err(|error| error.to_string())?;
            value_of(Ok(report))
        }
        ("fixture", Some("write-image")) => {
            let request: ImageRequest = options.request()?;
            value_of(write_image(&request))
        }
        ("fixture", Some("corpus")) => {
            let request: CorpusRequest = options.request_or_default()?;
            value_of(write_corpus(&request))
        }
        ("logs", Some("emit")) => {
            let request: LogEmitRequest = options.request_or_default()?;
            emit_logs(&request)
        }
        ("logs", Some("path")) => Ok(json!({
            "logDir": app::paths::log_dir().display().to_string(),
            "logPath": log_file_path().display().to_string(),
        })),
        ("import" | "metadata" | "folder" | "collection" | "history" | "viewer" | "edit", _) => {
            write::dispatch(app, options, cancelled)
        }
        ("task" | "maintenance" | "scenario", _) => maintenance::dispatch(app, options, cancelled),
        ("optimize", _) => optimize::dispatch(app, options, cancelled),
        ("ai", _) => ai::dispatch(app, options, cancelled),
        ("mcp", _) => mcp::dispatch(app, options, cancelled),
        (command, action) => Err(match action {
            Some(action) => format!("未知命令：{command} {action}"),
            None => format!("未知命令：{command}"),
        }),
    }
}

fn save_settings(state: &AppState, settings: Settings) -> Result<Settings, String> {
    let previous = state.settings.snapshot();
    let saved = AppError::command(state.settings.replace(settings))?;
    if previous.ai != saved.ai {
        state.ai.apply_settings(&previous.ai, &saved.ai);
    }
    if previous.mcp != saved.mcp {
        state.apply_mcp_settings(&saved.mcp);
    }
    if previous.performance.worker_threads != saved.performance.worker_threads {
        state
            .images()
            .update_from_setting(saved.performance.worker_threads);
    }
    state.after_history_write();
    Ok(saved)
}

fn open_library(app: &tauri::AppHandle, path: &Path) -> Result<LibraryInfo, String> {
    trace::write("open_library_path", || {
        let state = app.state::<AppState>();
        let state = state.inner();
        match state.inspect_library(path) {
            LibraryInspection::Library { .. } => AppError::command(state.open_library(path)),
            LibraryInspection::Empty => Err("目录不是图库，请先创建图库".to_owned()),
            LibraryInspection::Invalid { reason } => Err(format!("该目录无法作为图库：{reason}")),
        }
    })
}

pub(super) fn value_of<T: Serialize>(value: Result<T, String>) -> Result<Value, String> {
    serde_json::to_value(value?).map_err(|error| format!("无法序列化结果：{error}"))
}

fn log_file_path() -> PathBuf {
    app::paths::log_dir().join("lilith-artbank.log")
}

fn require_root() -> Result<PathBuf, String> {
    let Some(root) = std::env::var_os("LILITH_ARTBANK_ROOT") else {
        return Err("无头模式需要设置 LILITH_ARTBANK_ROOT，避免写入程序目录".into());
    };
    if root.is_empty() {
        return Err("LILITH_ARTBANK_ROOT 不能为空".into());
    }
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        return Err("LILITH_ARTBANK_ROOT 必须是绝对路径".into());
    }
    Ok(root)
}

pub(super) struct Options {
    pub(super) command: String,
    pub(super) action: Option<String>,
    pub(super) library: Option<PathBuf>,
    pub(super) result: Option<PathBuf>,
    pub(super) request: Option<Value>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let command = args[0].clone();
        let mut action = None;
        let mut library = None;
        let mut result = None;
        let mut request_file = None;
        let mut request_json = None;
        let mut index = 1;
        if index < args.len() && !args[index].starts_with("--") {
            action = Some(args[index].clone());
            index += 1;
        }
        while index < args.len() {
            let flag = args[index].as_str();
            let value = args
                .get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} 缺少取值"))?;
            match flag {
                "--library" => library = Some(PathBuf::from(value)),
                "--result" => result = Some(PathBuf::from(value)),
                "--request" => request_file = Some(value),
                "--request-json" => request_json = Some(value),
                other => return Err(format!("未知参数：{other}")),
            }
            index += 2;
        }
        if request_file.is_some() && request_json.is_some() {
            return Err("--request 与 --request-json 只能选一个".into());
        }
        let request = match (request_file, request_json) {
            (None, None) => None,
            (Some(path), None) => {
                let text = fs::read_to_string(&path)
                    .map_err(|error| format!("无法读取请求文件 {path}：{error}"))?;
                Some(
                    serde_json::from_str(&text)
                        .map_err(|error| format!("请求文件不是有效 JSON：{error}"))?,
                )
            }
            (None, Some(text)) => Some(
                serde_json::from_str(&text)
                    .map_err(|error| format!("--request-json 不是有效 JSON：{error}"))?,
            ),
            (Some(_), Some(_)) => unreachable!("上面已拒绝同时给出"),
        };
        Ok(Self {
            command,
            action,
            library,
            result,
            request,
        })
    }

    pub(super) fn request<T: DeserializeOwned>(&self) -> Result<T, String> {
        let value = self
            .request
            .clone()
            .ok_or_else(|| "该命令需要 --request 或 --request-json".to_owned())?;
        serde_json::from_value(value).map_err(|error| format!("请求字段无效：{error}"))
    }

    pub(super) fn request_or_default<T: DeserializeOwned + Default>(&self) -> Result<T, String> {
        match &self.request {
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|error| format!("请求字段无效：{error}")),
            None => Ok(T::default()),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryCreate {
    path: String,
    name: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryPath {
    path: String,
}

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct TagsRequest {
    search: String,
    limit: Option<u32>,
    include_empty: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageIdRequest {
    image_id: i64,
}

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ImageIdsRequest {
    image_ids: Vec<i64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ImageRequest {
    path: String,
    width: u32,
    height: u32,
    format: String,
    transparent: bool,
    /// 只用于夹具：按位翻转 4×4 格，让同尺寸样本内容不同。
    seed: u32,
    /// 只用于夹具：在前几个像素上做 1 级微调，用来造 pHash 相近的近似重复图。
    tweak: u32,
}

impl Default for ImageRequest {
    fn default() -> Self {
        Self {
            path: String::new(),
            width: 16,
            height: 16,
            format: "png".into(),
            transparent: false,
            seed: 0,
            tweak: 0,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CorpusRequest {
    directory: String,
    count: usize,
    width: u32,
    height: u32,
    formats: Vec<String>,
}

impl Default for CorpusRequest {
    fn default() -> Self {
        Self {
            directory: String::new(),
            count: 8,
            width: 16,
            height: 16,
            formats: vec!["png".into(), "jpg".into()],
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct LogEmitRequest {
    level: String,
    lines: usize,
    line_bytes: usize,
}

impl Default for LogEmitRequest {
    fn default() -> Self {
        Self {
            level: "info".into(),
            lines: 1,
            line_bytes: 16,
        }
    }
}

fn emit_logs(request: &LogEmitRequest) -> Result<Value, String> {
    let level = match request.level.trim().to_ascii_lowercase().as_str() {
        "trace" => log::Level::Trace,
        "debug" => log::Level::Debug,
        "info" => log::Level::Info,
        "warn" => log::Level::Warn,
        "error" => log::Level::Error,
        other => return Err(format!("未知日志等级：{other}")),
    };
    let line = "x".repeat(request.line_bytes.max(1));
    for _ in 0..request.lines.max(1) {
        log::log!(target: "acceptance", level, "{line}");
    }
    Ok(json!({
        "level": request.level,
        "lines": request.lines.max(1),
        "lineBytes": request.line_bytes.max(1),
    }))
}

fn write_image(request: &ImageRequest) -> Result<Value, String> {
    let path = Path::new(&request.path);
    if !path.is_absolute() {
        return Err("--request.path 必须是绝对路径".into());
    }
    let (bytes, extension) = render_image(request, request.seed)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建目录：{error}"))?;
    }
    fs::write(path, &bytes).map_err(|error| format!("无法写入图片：{error}"))?;
    Ok(json!({
        "path": path.display().to_string(),
        "format": extension,
        "width": request.width,
        "height": request.height,
        "bytes": bytes.len(),
    }))
}

fn write_corpus(request: &CorpusRequest) -> Result<Value, String> {
    let directory = Path::new(&request.directory);
    if !directory.is_absolute() {
        return Err("--request.directory 必须是绝对路径".into());
    }
    fs::create_dir_all(directory).map_err(|error| format!("无法创建样本目录：{error}"))?;
    let formats = if request.formats.is_empty() {
        vec!["png".to_owned(), "jpg".to_owned()]
    } else {
        request.formats.clone()
    };
    let mut files = Vec::with_capacity(request.count);
    // 每个样本目录给一个稳定偏移：同一目录重复生成仍得到同一批内容（判重可复现），
    // 不同目录之间则不会撞成同一批哈希。
    let base_seed = request.directory.bytes().fold(0_u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(byte as u32)
    });
    for index in 0..request.count {
        let format = &formats[index % formats.len()];
        let seed = base_seed.wrapping_add(index as u32);
        let (bytes, extension) = render_image(
            &ImageRequest {
                path: String::new(),
                width: request.width.max(1),
                height: request.height.max(1),
                format: format.clone(),
                transparent: index % 3 == 1,
                seed,
                tweak: 0,
            },
            seed,
        )?;
        let name = format!("sample-{index:03}.{extension}");
        fs::write(directory.join(&name), &bytes)
            .map_err(|error| format!("无法写入样本：{error}"))?;
        files.push(name);
    }
    Ok(json!({
        "directory": directory.display().to_string(),
        "count": files.len(),
        "files": files,
        "formats": formats,
    }))
}

fn render_image(request: &ImageRequest, seed: u32) -> Result<(Vec<u8>, String), String> {
    let format = image_format(&request.format)?;
    let width = request.width.max(1);
    let height = request.height.max(1);
    let mut buffer = RgbaImage::new(width, height);
    for (x, y, pixel) in buffer.enumerate_pixels_mut() {
        let mut red = (x * 255 / width) as u8;
        let mut green = (y * 255 / height) as u8;
        // 4×4 大格按种子位翻转明暗：让不同样本的感知哈希也有明显差异，
        // 否则导入会把一批渐变图当成相似重复。
        let cell = (y * 4 / height) * 4 + x * 4 / width;
        if (seed >> (cell % 16)) & 1 == 1 {
            red = 255 - red;
            green = 255 - green;
        }
        let blue = (seed.wrapping_mul(37).wrapping_add(96) % 256) as u8;
        let alpha = if request.transparent {
            ((x.wrapping_add(y).wrapping_add(seed)) % 256) as u8
        } else {
            255
        };
        *pixel = Rgba([red, green, blue, alpha]);
    }
    // 把种子写进前 4 个像素的最低位，保证不同样本即使渐变相同也不会撞哈希。
    if width >= 4 && height >= 1 {
        for index in 0..4_u32 {
            let byte = (seed >> (index * 8)) as u8;
            buffer.put_pixel(
                index,
                0,
                Rgba([byte, byte.wrapping_mul(3), byte.wrapping_mul(7), 255]),
            );
        }
    }
    // 近似重复图：只动右下角少量像素，pHash 仍与原图接近。
    for index in 0..request.tweak.min(width) {
        let x = width - 1 - index;
        let y = height - 1;
        let pixel = buffer.get_pixel(x, y);
        buffer.put_pixel(x, y, Rgba([pixel[0] ^ 1, pixel[1], pixel[2], pixel[3]]));
    }
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(buffer)
        .write_to(&mut cursor, format)
        .map_err(|error| format!("无法编码图片：{error}"))?;
    Ok((cursor.into_inner(), extension_of(format).to_owned()))
}

fn image_format(value: &str) -> Result<ImageFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "png" => Ok(ImageFormat::Png),
        "jpg" | "jpeg" => Ok(ImageFormat::Jpeg),
        "bmp" => Ok(ImageFormat::Bmp),
        "gif" => Ok(ImageFormat::Gif),
        "tga" => Ok(ImageFormat::Tga),
        "tif" | "tiff" => Ok(ImageFormat::Tiff),
        "webp" => Ok(ImageFormat::WebP),
        other => Err(format!("不支持的图片格式：{other}")),
    }
}

fn extension_of(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Gif => "gif",
        ImageFormat::Tga => "tga",
        ImageFormat::Tiff => "tiff",
        ImageFormat::WebP => "webp",
        _ => "bin",
    }
}

#[cfg(windows)]
fn peak_working_set_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.PeakWorkingSetSize as u64)
}

#[cfg(not(windows))]
fn peak_working_set_bytes() -> Option<u64> {
    None
}

fn usage() -> String {
    String::from(
        "lilith-artbank --headless <command> [action] [options]\n\
         \n\
         开发期无窗口验收入口，不进发布产物。不创建用户可见窗口、不加载 WebView。\n\
         \n\
         commands:\n\
         \x20 ping                              最小启动检查\n\
         \x20 library create|open|open-configured|close|info|counts|inspect-path\n\
         \x20 settings get|save\n\
         \x20 query images|ids|stats|folders|tags|detail|states|selection-tags\n\
         \x20 import run|start|cancel|status\n\
         \x20 metadata score|set-score|description|tags|add-tags|remove-tags\n\
         \x20          rename-tag|delete-tag|merge-tags|move|trash|restore|delete|empty-trash\n\
         \x20 folder create|rename|move|delete|restore|delete-permanently|reorder|place|apply-order|trash\n\
         \x20 collection list|create|rename|delete|reorder|add|remove|combine\n\
         \x20 history list|undo|redo|clear\n\
         \x20 viewer source\n\
         \x20 edit estimate|save\n\
         \x20 task list|wait|cancel\n\
         \x20 maintenance thumbnails|check|export|orphan-move|save-report|pack|compact|zero-tags\n\
         \x20 optimize estimate（执行走 scenario optimize_flow，估算凭证只存在进程内）\n\
         \x20 ai status|health|ensure|embed-text|shutdown|embed|pose|pose-get|describe\n\
         \x20    search|similar|groups|keeps|temp-features|features\n\
         \x20 mcp status\n\
         \x20 scenario import_cancel|task_queue_cancel|task_panic|close_during_task\n\
         \x20          epoch_switch|folder_lock|ai_lock|maintenance_check|idle_gc|idle_yield\n\
         \x20          mcp_bridge|mcp_modes|optimize_flow|ai_search_flow|ai_temp_flow\n\
         \x20 fixture seed-db|write-image|corpus\n\
         \x20 logs emit|path\n\
         \n\
         options:\n\
         \x20 --library <path>                 执行命令前显式打开图库\n\
         \x20 --request <json-file>            DTO 请求文件\n\
         \x20 --request-json <inline>          DTO 请求（内联 JSON）\n\
         \x20 --result <file>                  另存格式化结果\n\
         \n\
         必须设置 LILITH_ARTBANK_ROOT（绝对路径）。退出码：0 成功、1 失败、2 取消。\n",
    )
}

#[cfg(all(test, feature = "headless"))]
mod tests {
    use super::*;

    #[test]
    fn parses_library_and_request_options() {
        let options = Options::parse(&[
            "query".into(),
            "images".into(),
            "--library".into(),
            r"C:\library".into(),
            "--request-json".into(),
            r#"{"limit": 5}"#.into(),
        ])
        .unwrap();
        assert_eq!(options.command, "query");
        assert_eq!(options.action.as_deref(), Some("images"));
        assert_eq!(options.library.as_deref(), Some(Path::new(r"C:\library")));
        let query: LibraryQuery = options.request().unwrap();
        assert_eq!(query.limit, Some(5));
    }

    #[test]
    fn renders_small_png() {
        let request = ImageRequest {
            path: String::new(),
            width: 4,
            height: 3,
            format: "png".into(),
            transparent: false,
            seed: 0,
            tweak: 0,
        };
        let (bytes, extension) = render_image(&request, 0).unwrap();
        assert_eq!(extension, "png");
        assert!(bytes.starts_with(b"\x89PNG"));
    }
}

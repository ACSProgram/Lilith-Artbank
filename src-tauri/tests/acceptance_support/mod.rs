//! 无窗口验收共享助手：工作区、子进程编排、结果 JSON、报告与档位。
//!
//! 测试只通过命令行、stdout 与结果文件断言，不读取被测进程内部状态。所有会启动
//! 子进程的目标必须以 `#![cfg(feature = "headless")]` 开头；`gate_guard` 会检查。
//! 每个用例向 `<target>/acceptance/report.jsonl` 追加一行，失败用例由 `Report` 的
//! `Drop` 补写；`LILITH_ACCEPT_KEEP=1` 时工作区不回收并打印路径。
#![allow(dead_code)]

use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{Value, json};

pub const DEFAULT_HEADLESS_BIN: &str = env!("CARGO_BIN_EXE_lilith-artbank");
const POLL: Duration = Duration::from_millis(5);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const DEFAULT_DISK_BUDGET: u64 = 2 * GIB;

static SPAWN_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PROBE: OnceLock<()> = OnceLock::new();
static REPORT_LOCK: Mutex<()> = Mutex::new(());

/// 本次构建的 debug 二进制；`LILITH_ACCEPT_BIN` 可覆盖。
pub fn headless_bin() -> PathBuf {
    let path = std::env::var_os("LILITH_ACCEPT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_HEADLESS_BIN));
    assert!(
        path.is_file(),
        "被测二进制不存在：{}；先运行 cargo build --features headless",
        path.display()
    );
    PROBE.get_or_init(|| probe_headless(&path));
    path
}

fn probe_headless(path: &Path) {
    let mut child = Command::new(path)
        .arg("--headless")
        .arg("help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("无法启动被测二进制 {}：{error}", path.display()));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(
                    status.success(),
                    "被测二进制 {} 的 `--headless help` 未正常返回：{status}",
                    path.display()
                );
                return;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "被测二进制 {} 没有启用 headless feature（`--headless` 被忽略，已终止）。\
                     请用 `cargo build --features headless` 重新构建。",
                    path.display()
                );
            }
            Err(error) => panic!("探针等待失败：{error}"),
        }
    }
}

/// 本次构建 target 目录下的验收产物根：报告、账本与工作区都在这里。
pub fn output_directory() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("CARGO_TARGET_TMPDIR 必须位于 target 目录之内")
        .join("acceptance")
}

pub struct Workspace {
    _directory: Option<tempfile::TempDir>,
    root: PathBuf,
}

impl Workspace {
    pub fn new() -> Self {
        let base = output_directory().join("workspaces");
        fs::create_dir_all(&base).expect("无法创建验收工作区根");
        let directory = tempfile::Builder::new()
            .prefix(".accept-")
            .tempdir_in(&base)
            .expect("无法创建临时目录");
        let root = directory.path().join("workspace");
        for name in ["app", "library", "scratch", "out"] {
            fs::create_dir_all(root.join(name)).expect("无法创建工作区子目录");
        }
        let directory = if std::env::var_os("LILITH_ACCEPT_KEEP").is_some() {
            eprintln!("[keep] {}", root.display());
            std::mem::forget(directory);
            None
        } else {
            Some(directory)
        };
        Self {
            _directory: directory,
            root,
        }
    }

    /// 程序根目录，作为子进程的 `LILITH_ARTBANK_ROOT`。
    pub fn app_root(&self) -> PathBuf {
        self.root.join("app")
    }

    /// 测试图库目录（尚未创建，交给 `library create`）。
    pub fn library(&self) -> PathBuf {
        self.root.join("library")
    }

    pub fn scratch(&self) -> PathBuf {
        self.root.join("scratch")
    }

    pub fn out(&self, name: &str) -> PathBuf {
        self.root.join("out").join(name)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.app_root().join("logs")
    }

    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("lilith-artbank.log")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.app_root().join("settings.json")
    }

    pub fn assert_within_budget(&self) {
        let usage = tree_usage(&self.root);
        let budget = disk_budget();
        assert!(
            usage.bytes <= budget,
            "工作区 {} 占用 {}，超过预算 {}",
            self.root.display(),
            human_bytes(usage.bytes),
            human_bytes(budget)
        );
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Run<'a> {
    workspace: &'a Workspace,
    command: String,
    action: Option<String>,
    args: Vec<String>,
    request: Option<Value>,
    env: Vec<(String, String)>,
    timeout: Duration,
}

pub fn headless<'a>(workspace: &'a Workspace, command: &str) -> Run<'a> {
    Run {
        workspace,
        command: command.to_owned(),
        action: None,
        args: Vec::new(),
        request: None,
        env: Vec::new(),
        timeout: DEFAULT_TIMEOUT,
    }
}

impl<'a> Run<'a> {
    pub fn action(mut self, action: &str) -> Self {
        self.action = Some(action.to_owned());
        self
    }

    /// 以文件方式传 `--request`，避免命令行长度限制。
    pub fn request(mut self, value: impl Serialize) -> Self {
        self.request = Some(serde_json::to_value(value).expect("请求无法序列化"));
        self
    }

    pub fn library(mut self, path: &Path) -> Self {
        self.args.push("--library".into());
        self.args.push(path.to_string_lossy().into_owned());
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn finish(self) -> Outcome {
        let id = SPAWN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let scratch = self.workspace.scratch();
        let stem = format!("{}-{id}", self.command);
        let result_path = scratch.join(format!("{stem}.result.json"));
        let request_path = scratch.join(format!("{stem}.request.json"));
        let stdout_path = scratch.join(format!("{stem}.stdout"));
        let stderr_path = scratch.join(format!("{stem}.stderr"));

        let mut command = Command::new(headless_bin());
        command.arg("--headless").arg(&self.command);
        if let Some(action) = &self.action {
            command.arg(action);
        }
        if let Some(request) = &self.request {
            fs::write(
                &request_path,
                serde_json::to_vec_pretty(request).expect("请求无法序列化"),
            )
            .expect("无法写请求文件");
            command.arg("--request").arg(&request_path);
        }
        command.args(&self.args);
        command.arg("--result").arg(&result_path);
        command
            .env("LILITH_ARTBANK_ROOT", self.workspace.app_root())
            .current_dir(self.workspace.scratch())
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                File::create(&stdout_path).expect("无法创建 stdout 文件"),
            ))
            .stderr(Stdio::from(
                File::create(&stderr_path).expect("无法创建 stderr 文件"),
            ));
        for (key, value) in &self.env {
            command.env(key, value);
        }

        let started = Instant::now();
        let mut child = command.spawn().expect("无法启动无头进程");
        let deadline = started + self.timeout;
        let (status, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (status, false),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let status = child.wait().expect("无法等待被终止的无头进程");
                    break (status, true);
                }
                Ok(None) => std::thread::sleep(POLL),
                Err(error) => panic!("等待无头进程失败：{error}"),
            }
        };
        let elapsed = started.elapsed();
        let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
        let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
        let result = fs::read_to_string(&result_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .or_else(|| serde_json::from_str::<Value>(stdout.trim()).ok())
            .unwrap_or(Value::Null);
        Outcome {
            exit_code: status.code().unwrap_or(-1),
            timed_out,
            result,
            elapsed,
            stdout,
            stderr,
        }
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub exit_code: i32,
    pub timed_out: bool,
    pub result: Value,
    pub elapsed: Duration,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.result["ok"].as_bool().unwrap_or(false)
    }

    pub fn error(&self) -> String {
        self.result["error"].as_str().unwrap_or_default().to_owned()
    }

    pub fn data(&self) -> &Value {
        &self.result["data"]
    }

    pub fn peak_working_set_bytes(&self) -> u64 {
        self.result["peakWorkingSetBytes"].as_u64().unwrap_or(0)
    }

    pub fn describe(&self) -> String {
        format!(
            "exit_code={} timed_out={} elapsed_ms={} result={}\nstdout:\n{}\nstderr:\n{}",
            self.exit_code,
            self.timed_out,
            self.elapsed.as_millis(),
            serde_json::to_string(&self.result).unwrap_or_default(),
            self.stdout,
            self.stderr
        )
    }

    #[track_caller]
    pub fn expect_ok(&self) -> &Self {
        assert!(
            !self.timed_out && self.ok() && self.exit_code == 0,
            "期望成功但未成功：\n{}",
            self.describe()
        );
        self
    }

    #[track_caller]
    pub fn expect_cancelled(&self) -> &Self {
        assert!(
            !self.timed_out && self.ok() && self.exit_code == 2,
            "期望取消但未取消：\n{}",
            self.describe()
        );
        self
    }

    #[track_caller]
    pub fn expect_failed(&self, needle: &str) -> &Self {
        assert!(
            !self.timed_out && !self.ok() && self.exit_code == 1,
            "期望失败但未失败：\n{}",
            self.describe()
        );
        assert!(
            self.error().contains(needle),
            "错误信息不含 {needle:?}：\n{}",
            self.describe()
        );
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Cheap,
    Default,
    Heavy,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Cheap => "cheap",
            Tier::Default => "default",
            Tier::Heavy => "heavy",
        }
    }
}

/// `LILITH_ACCEPT_TIERS=cheap,default`；未设置时 cheap+default。
pub fn tier_enabled(tier: Tier) -> bool {
    static TIERS: OnceLock<Vec<Tier>> = OnceLock::new();
    TIERS
        .get_or_init(|| match std::env::var("LILITH_ACCEPT_TIERS") {
            Ok(value) if !value.trim().is_empty() => value
                .split(',')
                .map(str::trim)
                .map(|name| match name {
                    "cheap" => Tier::Cheap,
                    "default" => Tier::Default,
                    "heavy" => Tier::Heavy,
                    other => {
                        panic!("LILITH_ACCEPT_TIERS 只接受 cheap/default/heavy，收到 {other:?}")
                    }
                })
                .collect(),
            _ => vec![Tier::Cheap, Tier::Default],
        })
        .contains(&tier)
}

/// 档位未启用时打印一行并返回 true，测试据此提前返回。
pub fn skip_unless(tier: Tier, name: &str) -> bool {
    if tier_enabled(tier) {
        false
    } else {
        eprintln!("[skip] {name}（tier={}）", tier.as_str());
        true
    }
}

/// 测试级报告。断言失败 panic 时由 Drop 写一条 ok=false 记录。
pub struct Report {
    name: &'static str,
    tier: Tier,
    root: PathBuf,
    started: Instant,
    ok: bool,
    commands: usize,
    peak: u64,
    written: bool,
}

impl Report {
    pub fn new(name: &'static str, tier: Tier, workspace: &Workspace) -> Self {
        Self {
            name,
            tier,
            root: workspace.root.clone(),
            started: Instant::now(),
            ok: false,
            commands: 0,
            peak: 0,
            written: false,
        }
    }

    pub fn record(&mut self, outcome: &Outcome) {
        self.commands += 1;
        self.peak = self.peak.max(outcome.peak_working_set_bytes());
    }

    #[track_caller]
    pub fn success(mut self) {
        self.ok = true;
        self.write();
    }

    fn write(&mut self) {
        if self.written {
            return;
        }
        self.written = true;
        let usage = tree_usage(&self.root);
        let entry = json!({
            "at": now_ms(),
            "name": self.name,
            "tier": self.tier.as_str(),
            "ok": self.ok,
            "elapsedMs": self.started.elapsed().as_millis(),
            "peakWorkingSetBytes": self.peak,
            "commands": self.commands,
            "files": usage.files,
            "bytes": usage.bytes,
        });
        let _guard = REPORT_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = output_directory().join("report.jsonl");
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let line = serde_json::to_string(&entry).unwrap_or_default();
        if let Ok(mut handle) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(handle, "{line}");
        }
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        self.write();
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TreeUsage {
    pub files: u64,
    pub bytes: u64,
}

pub fn tree_usage(root: &Path) -> TreeUsage {
    let mut usage = TreeUsage::default();
    let Ok(entries) = fs::read_dir(root) else {
        return usage;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            let child = tree_usage(&path);
            usage.files += child.files;
            usage.bytes += child.bytes;
        } else if metadata.is_file() {
            usage.files += 1;
            usage.bytes += metadata.len();
        }
    }
    usage
}

pub fn disk_budget() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| match std::env::var("LILITH_ACCEPT_DISK_BUDGET") {
        Ok(value) if !value.trim().is_empty() => parse_byte_size(&value).unwrap_or_else(|| {
            panic!("LILITH_ACCEPT_DISK_BUDGET 无法解析：{value:?}（接受字节数或 KiB/MiB/GiB 后缀）")
        }),
        _ => DEFAULT_DISK_BUDGET,
    })
}

/// 创建测试图库并返回 (目录, 图库 id)。
pub fn create_library(workspace: &Workspace) -> (PathBuf, String) {
    let library = workspace.library();
    let outcome = headless(workspace, "library")
        .action("create")
        .request(json!({
            "path": library.to_string_lossy(),
            "name": "验收图库",
        }))
        .finish();
    outcome.expect_ok();
    let id = outcome.data()["id"].as_str().expect("图库 id").to_owned();
    (library, id)
}

/// 向已创建的图库写入合成行与可选占位文件。
pub fn seed_library(workspace: &Workspace, library: &Path, spec: Value) -> Value {
    let outcome = headless(workspace, "fixture")
        .action("seed-db")
        .library(library)
        .request(spec)
        .finish();
    outcome.expect_ok();
    outcome.data().clone()
}

/// 当前图库全部图片 id（默认视图）。
pub fn image_ids(workspace: &Workspace, library: &Path) -> Vec<i64> {
    let outcome = headless(workspace, "query")
        .action("ids")
        .library(library)
        .finish();
    outcome.expect_ok();
    outcome
        .data()
        .as_array()
        .expect("id 列表")
        .iter()
        .map(|value| value.as_i64().expect("图片 id"))
        .collect()
}

pub fn folder_list(workspace: &Workspace, library: &Path) -> Vec<Value> {
    let outcome = headless(workspace, "query")
        .action("folders")
        .library(library)
        .finish();
    outcome.expect_ok();
    outcome.data().as_array().cloned().unwrap_or_default()
}

pub fn history_entries(workspace: &Workspace, library: &Path) -> Vec<Value> {
    let outcome = headless(workspace, "history")
        .action("list")
        .library(library)
        .finish();
    outcome.expect_ok();
    outcome.data()["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// 向 `directory` 写 `count` 张合成样本，格式按 `formats` 轮转；返回文件名。
pub fn write_corpus(
    workspace: &Workspace,
    report: &mut Report,
    directory: &Path,
    count: usize,
    formats: &[&str],
) -> Vec<String> {
    let outcome = headless(workspace, "fixture")
        .action("corpus")
        .request(json!({
            "directory": directory.to_string_lossy(),
            "count": count,
            "formats": formats,
        }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    let files: Vec<String> = outcome.data()["files"]
        .as_array()
        .expect("文件列表")
        .iter()
        .map(|name| name.as_str().expect("文件名").to_owned())
        .collect();
    assert_eq!(files.len(), count, "样本数不符");
    for name in &files {
        assert!(directory.join(name).is_file(), "缺少样本 {name}");
    }
    files
}

/// 造 `count` 张 32×24 PNG 导入到 `folder`，返回导入后的全部图片 id。
pub fn import_images(
    workspace: &Workspace,
    library: &Path,
    report: &mut Report,
    count: usize,
    folder: &str,
) -> Vec<i64> {
    let corpus = workspace.scratch().join("images");
    let corpus_outcome = headless(workspace, "fixture")
        .action("corpus")
        .request(json!({
            "directory": corpus.to_string_lossy(),
            "count": count,
            "width": 32,
            "height": 24,
            "formats": ["png"],
        }))
        .finish();
    report.record(&corpus_outcome);
    corpus_outcome.expect_ok();
    let imported = headless(workspace, "import")
        .action("run")
        .library(library)
        .request(json!({ "paths": [corpus.to_string_lossy()], "folder": folder }))
        .finish();
    report.record(&imported);
    imported.expect_ok();
    assert_eq!(imported.data()["job"]["imported"], count);
    image_ids(workspace, library)
}

/// 图库计数（`imageCount` / `trashCount` / `tempCount`）。
pub fn library_counts(workspace: &Workspace, library: &Path, report: &mut Report) -> Value {
    let outcome = headless(workspace, "library")
        .action("counts")
        .library(library)
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    outcome.data().clone()
}

/// 按路径找文件夹 id，根文件夹传 `""`。
pub fn folder_id(workspace: &Workspace, library: &Path, report: &mut Report, path: &str) -> i64 {
    let outcome = headless(workspace, "query")
        .action("folders")
        .library(library)
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    outcome
        .data()
        .as_array()
        .expect("文件夹列表")
        .iter()
        .find(|folder| folder["path"] == path)
        .unwrap_or_else(|| panic!("文件夹 {path:?} 不存在"))["id"]
        .as_i64()
        .expect("文件夹 id")
}

pub fn parse_byte_size(value: &str) -> Option<u64> {
    let text = value.trim();
    let upper = text.to_ascii_uppercase();
    for (suffix, multiplier) in [
        ("TIB", 1024 * GIB),
        ("GIB", GIB),
        ("MIB", MIB),
        ("KIB", 1024),
        ("B", 1),
    ] {
        if let Some(digits) = upper.strip_suffix(suffix) {
            let number: u64 = digits.trim().parse().ok()?;
            return number.checked_mul(multiplier);
        }
    }
    text.parse().ok()
}

pub fn human_bytes(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}

/// 读取日志文件；不存在时返回空串（测试自行断言）。
pub fn read_log(workspace: &Workspace) -> String {
    fs::read_to_string(workspace.log_file()).unwrap_or_default()
}

/// 读取图库 `settings.json`。
pub fn read_settings(workspace: &Workspace) -> Value {
    fs::read_to_string(workspace.settings_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

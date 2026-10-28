//! 日志格式、运行标识与命令记录。
//!
//! 这里只承载「常规档」：事件驱动的记录，常开、平时零噪声。周期性探针、取证通道
//! 与看门狗属于诊断档，不在本模块内。
//!
//! 三件事：
//!
//! 1. **等级**：基线由 `LILITH_ARTBANK_LOG_LEVEL` 指定，未指定时调试构建 Debug、
//!    发布构建 Info。`lib.rs` 刻意不在插件上调用 `.level()`——插件会拿 fern dispatch
//!    的等级当全局 `max_level`，一旦固定，运行时再改也放不出更低等级。因此
//!    [`baseline_level`] + `log::set_max_level` 是唯一的等级开关，且必须写在应用
//!    `setup` 里：日志插件在 `setup` 之前初始化并自己调用过一次 `set_max_level`。
//! 2. **行格式**：本地时间 + `[run:xxxxxx]` + `[等级]` + `[target]`。运行标识让按大小
//!    轮转出来的文件仍能按单次运行切开；时间用本地时区，便于和用户描述的时间对齐。
//! 3. **命令记录**：写命令成对记录 `started` / `finished`（含耗时），读命令只在失败
//!    或超过阈值时记录。分档理由见 [`write`] 与 [`read`]。

use std::future::Future;
use std::sync::OnceLock;
use std::time::Instant;

use log::{LevelFilter, Record};
use tauri_plugin_log::TimezoneStrategy;
use tauri_plugin_log::fern::FormatCallback;

/// 读命令的慢阈值。元数据分页预算 50ms，超过 500ms 已是一个数量级的偏离。
const SLOW_READ_MS: u64 = 500;

/// 语义搜索与找相似的慢阈值。两者在大库上天然要几百毫秒到数秒，用读命令的
/// 500ms 会变成每次搜索都告警。
pub(crate) const SLOW_SEARCH_MS: u64 = 3_000;

/// 命令记录的 target。用固定值而不是模块路径，便于 `grep '\[command\]'`。
const COMMAND_TARGET: &str = "command";

/// 本次运行的短标识。
///
/// 写进每一行日志，便于从按大小轮转的文件里切出单次运行：`grep '\[run:ab12cd\]'`。
/// 取值来自启动时刻低位与进程号的异或，不保证全局唯一，只要求同一次运行内稳定、
/// 不同次运行大概率不同。
static RUN_ID: OnceLock<String> = OnceLock::new();

pub(crate) fn run_id() -> &'static str {
    RUN_ID.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        format!(
            "{:06x}",
            (nanos ^ u64::from(std::process::id())) & 0x00ff_ffff
        )
    })
}

/// 基线等级：环境变量优先，否则调试构建 Debug、发布构建 Info。
///
/// 结果必须写进 `log::set_max_level`，而且要写在应用 `setup` 里——日志插件在
/// `setup` 之前初始化，会用 fern dispatch 的等级（默认 Trace）覆盖掉早先设置的
/// `max_level`。
pub(crate) fn baseline_level() -> LevelFilter {
    if let Ok(value) = std::env::var("LILITH_ARTBANK_LOG_LEVEL") {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => return LevelFilter::Off,
            "error" => return LevelFilter::Error,
            "warn" => return LevelFilter::Warn,
            "info" => return LevelFilter::Info,
            "debug" => return LevelFilter::Debug,
            "trace" => return LevelFilter::Trace,
            _ => {}
        }
    }
    if cfg!(debug_assertions) {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    }
}

/// 日志行格式。
///
/// 插件默认格式固定用 UTC（`DEFAULT_TIMEZONE_STRATEGY`），这里显式取本地时区。
/// `Builder::timezone_strategy` 会把 dispatch 的格式重置成插件默认格式，所以
/// `lib.rs` 里它必须排在本函数之前，否则这份格式会被丢掉，`[run:]` 也就不见了。
/// 毫秒不放进前缀，需要精度的地方由消息里的 `elapsed_ms` 承担。
pub(crate) fn format(out: FormatCallback, message: &std::fmt::Arguments<'_>, record: &Record<'_>) {
    let now = TimezoneStrategy::UseLocal.get_now();
    out.finish(format_args!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}[run:{}][{}][{}] {}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        run_id(),
        record.level(),
        record.target(),
        message,
    ))
}

/// 记录一次写命令：进入与结束各一行，失败记 `warn`。
///
/// 写命令由用户操作驱动，频率低，成对记录才有价值——进程若在操作中途崩溃，只有
/// `started` 没有 `finished` 就是现场证据。闭包形式让命令体里的 `?` 仍然正确：
/// 提前返回发生在闭包内，`finish` 依旧会执行。
pub(crate) fn write<T>(name: &str, run: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    log::info!(target: COMMAND_TARGET, "command started: name={name}");
    let started = Instant::now();
    let result = run();
    finish(name, started, result)
}

/// [`write`] 的异步版本。计时覆盖整个 future 的执行，含排队等待。
pub(crate) async fn write_async<T>(
    name: &str,
    run: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    log::info!(target: COMMAND_TARGET, "command started: name={name}");
    let started = Instant::now();
    let result = run.await;
    finish(name, started, result)
}

/// 记录一次读命令：成功且不慢时什么都不写，失败与超阈值记 `warn`。
///
/// 读命令里有随滚动与翻页反复触发的那几个（`query_images`、`get_image_states`、
/// `get_viewer_source`），成功路径必须保持零落盘，否则日志会被读路径刷满。
pub(crate) fn read<T>(name: &str, run: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let started = Instant::now();
    let result = run();
    report_read(name, started, SLOW_READ_MS, result)
}

/// [`read`] 的异步版本。
pub(crate) async fn read_async<T>(
    name: &str,
    run: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    read_async_with(name, SLOW_READ_MS, run).await
}

/// 慢阈值可调的读命令，供语义搜索与找相似使用。
pub(crate) async fn read_async_with<T>(
    name: &str,
    threshold_ms: u64,
    run: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let started = Instant::now();
    let result = run.await;
    report_read(name, started, threshold_ms, result)
}

fn finish<T>(name: &str, started: Instant, result: Result<T, String>) -> Result<T, String> {
    let elapsed = started.elapsed().as_millis();
    match &result {
        Ok(_) => {
            log::info!(
                target: COMMAND_TARGET,
                "command finished: name={name} elapsed_ms={elapsed} outcome=ok"
            );
        }
        Err(error) => {
            log::warn!(
                target: COMMAND_TARGET,
                "command failed: name={name} elapsed_ms={elapsed} error={error}"
            );
        }
    }
    result
}

fn report_read<T>(
    name: &str,
    started: Instant,
    threshold_ms: u64,
    result: Result<T, String>,
) -> Result<T, String> {
    let elapsed = started.elapsed().as_millis();
    match &result {
        Err(error) => {
            log::warn!(
                target: COMMAND_TARGET,
                "command failed: name={name} elapsed_ms={elapsed} error={error}"
            );
        }
        Ok(_) if elapsed > u128::from(threshold_ms) => {
            log::warn!(
                target: COMMAND_TARGET,
                "command slow: name={name} elapsed_ms={elapsed} threshold_ms={threshold_ms}"
            );
        }
        Ok(_) => {}
    }
    result
}

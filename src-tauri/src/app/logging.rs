//! 启动日志装配：GUI 与无头验收共用同一套目标、格式与轮转参数。
//!
//! 目标必须用 `targets()` 显式替换插件默认目标；`target()` 是在默认的
//! Stdout + LogDir 上追加，会留下重复 stdout 与一份 AppData 日志。

use std::{path::Path, path::PathBuf};

use tauri::Runtime;
use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};

/// 单个日志文件的上限。超过即轮转。
pub(crate) const LOG_MAX_FILE_SIZE: u128 = 4_000_000;
/// 保留的归档份数；活动文件不计入，因此 `logs/` 最多 6 个文件、约 24MB。
pub(crate) const LOG_KEPT_FILES: usize = 5;
const LOG_FILE_STEM: &str = "lilith-artbank";

/// 日志插件。无头入口不写 stdout，保证 stdout 只有单行结果 JSON。
pub(crate) fn plugin<R: Runtime>(log_dir: PathBuf, stdout: bool) -> tauri::plugin::TauriPlugin<R> {
    let mut targets = vec![Target::new(TargetKind::Folder {
        path: log_dir,
        file_name: Some(LOG_FILE_STEM.into()),
    })];
    if stdout {
        targets.push(Target::new(TargetKind::Stdout));
    }
    tauri_plugin_log::Builder::new()
        .targets(targets)
        // 顺序不能换：`timezone_strategy` 会把 dispatch 的格式重置成插件默认格式，
        // 必须排在 `format` 之前。
        .timezone_strategy(TimezoneStrategy::UseLocal)
        .format(crate::app::trace::format)
        .max_file_size(LOG_MAX_FILE_SIZE)
        .rotation_strategy(RotationStrategy::KeepSome(LOG_KEPT_FILES))
        .build()
}

/// 启动横幅。必须在 `log::set_max_level` 之后调用，否则随等级被过滤。
pub(crate) fn startup_banner(root: &Path) {
    log::info!(
        "Lilith Artbank starting: version={}, root={}, run={}, level={}, log_dir={}, rotation={}MB×{}",
        env!("CARGO_PKG_VERSION"),
        root.display(),
        crate::app::trace::run_id(),
        crate::app::trace::baseline_level()
            .as_str()
            .to_ascii_lowercase(),
        crate::app::paths::log_dir().display(),
        LOG_MAX_FILE_SIZE / 1_000_000,
        LOG_KEPT_FILES,
    );
}

mod ai;
mod app;
mod concurrency;
mod imageops;
mod import;
mod library;
mod mcp;
mod platform;

use tauri::Manager;

/// 单个日志文件的上限。超过即轮转。
const LOG_MAX_FILE_SIZE: u128 = 4_000_000;
/// 保留的归档份数；活动文件不计入，因此 `logs/` 最多 6 个文件、约 24MB。
const LOG_KEPT_FILES: usize = 5;

/// `--mcp-stdio` 分支：在 Tauri 初始化与 single-instance 插件之前接管 stdio。
pub fn run_stdio_bridge() -> i32 {
    mcp::bridge::run()
}

pub fn run() {
    let log_dir = app::paths::log_dir();
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        eprintln!("无法创建日志目录 {}：{error}", log_dir.display());
    }
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(
            |application, _args, _cwd| {
                if let Some(window) = application.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            },
        ))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            // 刻意不调用 `.level()`：fern 的 dispatch 等级会被插件拿去当全局 `max_level`，
            // 固定之后运行时再改也放不出更低等级。等级只由 `setup` 里的 `set_max_level`
            // 控制，见 `app/trace.rs` 的模块说明。
            tauri_plugin_log::Builder::new()
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Folder {
                        path: log_dir,
                        file_name: Some("lilith-artbank".into()),
                    },
                ))
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Stdout,
                ))
                // 顺序不能换：`timezone_strategy` 会把 dispatch 的格式重置成插件默认格式，
                // 必须排在 `format` 之前。它在这里只负责轮转文件名里的日期，行内时间戳
                // 由 `trace::format` 自己取本地时区。
                .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
                .format(app::trace::format)
                .max_file_size(LOG_MAX_FILE_SIZE)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(LOG_KEPT_FILES))
                .build(),
        );
    let builder = app::thumb_protocol::register(builder);
    let builder = app::img_protocol::register(builder);

    let application = builder
        .setup(|application| {
            // 等级必须在这里设，不能放在 `run()` 开头：日志插件在 `setup` 之前初始化，
            // 它会用 fern dispatch 的等级（默认 Trace）调用 `log::set_max_level`，
            // 早设的基线会被覆盖，`LILITH_ARTBANK_LOG_LEVEL` 也就失效了。
            log::set_max_level(app::trace::baseline_level());
            // 保留默认输出，同时把位置与消息写进程序日志；release 不再 abort，任务线程可以兜底。
            let default_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                log::error!("panic: {info}");
                default_hook(info);
            }));
            let root = app::paths::app_root();
            // 启动横幅放在最前：图库打开时的日志（schema 升级、无法加载）都排在它之后，
            // 一次运行的上下文才能从第一行读到。
            log::info!(
                "Lilith Artbank starting: version={}, root={}, run={}, level={}, log_dir={}, rotation={}MB×{}",
                env!("CARGO_PKG_VERSION"),
                root.display(),
                app::trace::run_id(),
                app::trace::baseline_level().as_str().to_ascii_lowercase(),
                app::paths::log_dir().display(),
                LOG_MAX_FILE_SIZE / 1_000_000,
                LOG_KEPT_FILES,
            );
            let settings_path = app::paths::settings_path();
            if let Some(parent) = settings_path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    std::io::Error::other(format!(
                        "无法在程序目录创建设置文件（{}）：{error}。请把程序放到可写目录。",
                        parent.display()
                    ))
                })?;
            }
            let settings = app::settings::SettingsStore::load_or_create(settings_path)?;
            let state = app::state::AppState::new(settings, application.handle().clone());
            state.open_configured_library();
            application.manage(state);
            application.state::<app::state::AppState>().after_manage();
            // autoStart 在后台触发；健康检查不阻塞窗口出现。
            let handle = application.handle().clone();
            std::thread::spawn(move || {
                let state = handle.state::<app::state::AppState>();
                let settings = state.settings.snapshot().ai;
                let _ = state.ai.status(&settings);
            });
            Ok(())
        })
        .invoke_handler(app::commands::handler())
        .build(tauri::generate_context!())
        .expect("error while building Lilith Artbank");

    application.run(|app_handle, event| {
        if let tauri::RunEvent::Ready = event {
            app::window_state::ensure_main_window_visible(app_handle);
        }
        if let tauri::RunEvent::Exit = event {
            let state = app_handle.state::<app::state::AppState>();
            let settings = state.settings.snapshot().ai;
            state.shutdown();
            state.ai.shutdown(&settings);
            state.mcp.stop();
        }
    });
}

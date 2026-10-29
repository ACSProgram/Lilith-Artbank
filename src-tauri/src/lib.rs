mod ai;
mod app;
mod concurrency;
#[cfg(feature = "headless")]
mod headless;
mod imageops;
mod import;
mod library;
mod mcp;
mod platform;

use tauri::Manager;

/// `--headless` 分支：不创建用户可见窗口，只装配状态并执行验收命令。
#[cfg(feature = "headless")]
pub fn run_headless(args: &[String]) -> i32 {
    headless::run(args)
}

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
        .plugin(app::logging::plugin(log_dir, true));
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
            app::logging::startup_banner(&root);
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

mod ai;
mod app;
mod imageops;
mod import;
mod library;
mod mcp;
mod platform;

use tauri::Manager;

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
        .plugin(tauri_plugin_dialog::init())
        .plugin(
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
                .level(log::LevelFilter::Info)
                .max_file_size(4_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(5))
                .build(),
        );
    let builder = app::thumb_protocol::register(builder);
    let builder = app::img_protocol::register(builder);

    let application = builder
        .setup(|application| {
            let root = app::paths::app_root();
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
            log::info!(
                "Lilith Artbank starting: version={}, root={}",
                env!("CARGO_PKG_VERSION"),
                root.display()
            );
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
        if let tauri::RunEvent::Exit = event {
            let state = app_handle.state::<app::state::AppState>();
            let settings = state.settings.snapshot().ai;
            state.shutdown();
            state.ai.shutdown(&settings);
            state.mcp.stop();
        }
    });
}

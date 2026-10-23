mod ai;
mod app;
mod imageops;
mod import;
mod library;
mod platform;

use tauri::Manager;

pub fn run() {
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
                    tauri_plugin_log::TargetKind::LogDir {
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
            let config_dir = application.path().app_config_dir()?;
            std::fs::create_dir_all(&config_dir)?;
            let settings =
                app::settings::SettingsStore::load_or_create(config_dir.join("settings.json"))?;
            application.manage(app::state::AppState::new(settings));
            log::info!(
                "Lilith Artbank starting: version={}, config={}",
                env!("CARGO_PKG_VERSION"),
                config_dir.display()
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
            state.ai.shutdown(&settings);
        }
    });
}

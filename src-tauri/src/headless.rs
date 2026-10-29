//! 无窗口验收入口（`feature = "headless"`）。
//!
//! 只服务于开发期验收：不创建用户可见窗口、不加载 WebView、不经过 IPC。当前只有
//! `ping` 用来确认最小启动链路，后续命令按计划逐步加入。

use std::{fs, io::Write, time::Instant};

use serde_json::{Value, json};
use tauri::Manager;

use crate::app::{self, settings::SettingsStore, state::AppState};

type HeadlessResult<T> = std::result::Result<T, String>;

pub fn run(args: &[String]) -> i32 {
    let started = Instant::now();
    match dispatch(args, started) {
        Ok(()) => 0,
        Err(error) => {
            let payload = json!({
                "command": args.first().map(String::as_str).unwrap_or(""),
                "ok": false,
                "error": error,
            });
            println!("{payload}");
            let _ = std::io::stdout().flush();
            1
        }
    }
}

fn dispatch(args: &[String], started: Instant) -> HeadlessResult<()> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("缺少命令；当前支持：ping".into());
    };
    if command != "ping" {
        return Err(format!("未知命令：{command}；当前支持：ping"));
    }

    let hold_ms = parse_hold_ms(&args[1..])?;
    let root = require_root()?;
    let settings_path = app::paths::settings_path();
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建设置目录 {}：{error}", parent.display()))?;
    }
    let settings = SettingsStore::load_or_create(settings_path.clone())
        .map_err(|error| format!("无法加载设置：{error}"))?;

    let app = tauri::Builder::default()
        .build(tauri::generate_context!())
        .map_err(|error| format!("无法创建无窗口应用：{error}"))?;
    let state = AppState::new(settings, app.handle().clone());
    let _ = app.manage(state);
    app.state::<AppState>().after_manage();

    let library = app
        .state::<AppState>()
        .status()
        .map_err(|error| format!("无法读取图库状态：{error}"))?;
    let payload: Value = json!({
        "command": "ping",
        "ok": true,
        "appRoot": root.display().to_string(),
        "settingsPath": settings_path.display().to_string(),
        "library": library,
        "pid": std::process::id(),
        "elapsedMs": started.elapsed().as_millis(),
    });
    println!("{payload}");
    let _ = std::io::stdout().flush();

    if hold_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(hold_ms));
    }

    {
        let state = app.state::<AppState>();
        state.shutdown();
        let ai = state.settings.snapshot().ai;
        state.ai.shutdown(&ai);
        state.mcp.stop();
    }
    drop(app);
    Ok(())
}

fn require_root() -> HeadlessResult<std::path::PathBuf> {
    let Some(root) = std::env::var_os("LILITH_ARTBANK_ROOT") else {
        return Err("无头模式需要设置 LILITH_ARTBANK_ROOT，避免写入程序目录".into());
    };
    if root.is_empty() {
        return Err("LILITH_ARTBANK_ROOT 不能为空".into());
    }
    let root = std::path::PathBuf::from(root);
    if !root.is_absolute() {
        return Err("LILITH_ARTBANK_ROOT 必须是绝对路径".into());
    }
    Ok(root)
}

fn parse_hold_ms(args: &[String]) -> HeadlessResult<u64> {
    let mut hold_ms = 0;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--hold-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--hold-ms 需要毫秒数".into());
                };
                hold_ms = value
                    .parse::<u64>()
                    .map_err(|_| format!("--hold-ms 不是有效毫秒数：{value}"))?;
                index += 2;
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(hold_ms)
}

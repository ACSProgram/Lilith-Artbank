use std::io::Write;

use crate::app::paths;

/// stdio 小桥不能写 stdout（那是 MCP 协议通道），诊断信息统一追加到 logs/mcp.log。
pub fn log_line(scope: &str, message: &str) {
    let path = paths::mcp_log_path();
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("无法创建 MCP 日志目录 {}：{error}", parent.display());
        return;
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{stamp} [{scope}] {message}"));
    if let Err(error) = result {
        eprintln!("无法写入 MCP 日志 {}：{error}", path.display());
    }
}

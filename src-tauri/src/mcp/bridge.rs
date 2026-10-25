use std::{
    io::{self, BufRead, Write},
    time::Duration,
};

use serde_json::{Value, json};

use super::{audit, runtime};
use crate::app::error::{AppError, AppResult};

/// stateless 会话没有握手时声明的协议版本；与 rmcp 的 `LATEST` 保持一致。
const LATEST_STATELESS_VERSION: &str = "2026-07-28";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// `lilith-artbank.exe --mcp-stdio`：把 stdio 上的 MCP JSON-RPC 逐行代理到应用内的 HTTP 服务。
///
/// stdout 只写协议消息；诊断写入 `logs/mcp.log`，不初始化 Tauri。
pub fn run() -> i32 {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build();
    let mut session = Session::default();

    audit::log_line("bridge", "stdio 小桥已启动");
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                audit::log_line("bridge", &format!("读取 stdin 失败：{error}"));
                return 1;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Err(error) = handle_line(line, &agent, &mut session, &mut stdout) {
            audit::log_line("bridge", &format!("代理请求失败：{error}"));
            let _ = write_message(
                &mut stdout,
                &json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32603, "message": error.to_string() },
                }),
            );
        }
    }
    audit::log_line("bridge", "stdio 小桥已退出");
    0
}

#[derive(Default)]
struct Session {
    protocol_version: Option<String>,
}

fn handle_line(
    line: &str,
    agent: &ureq::Agent,
    session: &mut Session,
    stdout: &mut impl Write,
) -> AppResult<()> {
    let message: Value = serde_json::from_str(line)
        .map_err(|error| AppError::message(format!("JSON 解析失败：{error}")))?;
    if message.is_array() {
        return Err(AppError::message("不支持 JSON-RPC 批量请求"));
    }
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned();

    let runtime = match runtime::read_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            if let Some(id) = id {
                write_error(stdout, id, -32000, &error.to_string())?;
            }
            return Ok(());
        }
    };
    let url = format!("http://127.0.0.1:{}/mcp", runtime.port);
    let mut request = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", runtime.token))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream");
    if method != "initialize" {
        // 服务端是 stateless 会话：没有握手时按协议最新版本声明。
        let version = session
            .protocol_version
            .as_deref()
            .unwrap_or(LATEST_STATELESS_VERSION);
        request = request.set("MCP-Protocol-Version", version);
    }

    let response = match request.send_string(line) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let body = response.into_string().unwrap_or_default();
            audit::log_line(
                "bridge",
                &format!("HTTP {status} method={method} body={body}"),
            );
            if let Some(id) = id {
                write_http_error(stdout, id, status, &body)?;
            }
            return Ok(());
        }
        Err(ureq::Error::Transport(error)) => {
            let message = format!("无法连接 Lilith Artbank MCP 服务：{error}");
            audit::log_line("bridge", &message);
            if let Some(id) = id {
                write_error(stdout, id, -32000, &message)?;
            }
            return Ok(());
        }
    };

    let content_type = response
        .header("Content-Type")
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = response
        .into_string()
        .map_err(|error| AppError::message(format!("读取 MCP 响应失败：{error}")))?;

    if content_type.starts_with("text/event-stream") {
        for payload in sse_payloads(&body) {
            if method == "initialize" {
                remember_protocol_version(&payload, session);
            }
            write_message(stdout, &payload)?;
        }
        return Ok(());
    }

    if body.trim().is_empty() {
        if let Some(id) = id {
            write_error(stdout, id, -32603, "MCP 服务没有返回结果")?;
        }
        return Ok(());
    }
    let payload: Value = serde_json::from_str(&body)
        .map_err(|error| AppError::message(format!("MCP 服务返回了无效 JSON：{error}")))?;
    if method == "initialize" {
        remember_protocol_version(&payload, session);
    }
    write_message(stdout, &payload)
}

fn remember_protocol_version(payload: &Value, session: &mut Session) {
    if let Some(version) = payload
        .get("result")
        .and_then(|result| result.get("protocolVersion"))
        .and_then(Value::as_str)
    {
        session.protocol_version = Some(version.to_owned());
    }
}

fn sse_payloads(body: &str) -> Vec<Value> {
    let mut payloads = Vec::new();
    let mut data = Vec::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim_start().to_owned());
        } else if line.trim().is_empty() && !data.is_empty() {
            let joined = data.join("\n");
            if let Ok(value) = serde_json::from_str(&joined) {
                payloads.push(value);
            }
            data.clear();
        }
    }
    if !data.is_empty()
        && let Ok(value) = serde_json::from_str(&data.join("\n"))
    {
        payloads.push(value);
    }
    payloads
}

fn write_http_error(stdout: &mut impl Write, id: Value, status: u16, body: &str) -> AppResult<()> {
    if let Ok(mut payload) = serde_json::from_str::<Value>(body)
        && payload.get("error").is_some()
    {
        payload["id"] = id;
        return write_message(stdout, &payload);
    }
    write_error(
        stdout,
        id,
        -32000,
        &format!("MCP 服务返回 HTTP {status}：{}", body.trim()),
    )
}

fn write_error(stdout: &mut impl Write, id: Value, code: i64, message: &str) -> AppResult<()> {
    write_message(
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    )
}

fn write_message(stdout: &mut impl Write, message: &Value) -> AppResult<()> {
    let bytes = serde_json::to_vec(message)
        .map_err(|error| AppError::message(format!("序列化 MCP 消息失败：{error}")))?;
    stdout.write_all(&bytes)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse_data_events() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":2}\n\n";
        let payloads = sse_payloads(body);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[1]["id"], 2);
    }

    #[test]
    fn joins_multiline_sse_data() {
        let body = "data: {\"a\":1,\ndata: \"b\":2}\n\n";
        let payloads = sse_payloads(body);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["b"], 2);
    }

    #[test]
    fn remembers_protocol_version_from_initialize() {
        let mut session = Session::default();
        remember_protocol_version(
            &json!({"result":{"protocolVersion":"2025-06-18"}}),
            &mut session,
        );
        assert_eq!(session.protocol_version.as_deref(), Some("2025-06-18"));
    }

    #[test]
    fn fallback_version_tracks_rmcp_latest() {
        assert_eq!(
            LATEST_STATELESS_VERSION,
            rmcp::model::ProtocolVersion::LATEST.as_str()
        );
    }
}

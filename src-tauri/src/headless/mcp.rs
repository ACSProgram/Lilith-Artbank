//! MCP：服务状态、三档权限与确认队列，以及真实 `--mcp-stdio` 小桥的场景编排。
//!
//! 场景自己拉起小桥子进程，按行发送 JSON-RPC；断言仍留在 `tests/`。

use std::{
    cell::Cell,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::Manager;

use crate::{
    app::{
        error::AppError,
        paths,
        settings::{McpMode, McpSettings},
        state::AppState,
    },
    mcp::runtime::McpStatus,
};

use super::{Options, value_of};

pub(super) fn dispatch(
    app: &tauri::AppHandle,
    options: &Options,
    _cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    match options.action.as_deref() {
        Some("status") => value_of(Ok(state.inner().mcp_status())),
        action => Err(match action {
            Some(action) => format!("未知命令：mcp {action}"),
            None => "未知命令：mcp".to_owned(),
        }),
    }
}

pub(super) fn scenario_bridge(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let request: BridgeRequest = options.request()?;
    if request.port == 0 {
        return Err("场景需要显式 port".into());
    }
    set_mcp(app, request.port, McpMode::Full, true)?;
    wait_running(app, Duration::from_secs(10))?;

    let mut bridge = Bridge::start()?;
    let initialize = bridge.request("initialize", initialize_params())?;
    let tools = bridge.request("tools/list", json!({}))?;
    let info = bridge.call_tool("library_info", json!({}))?;
    let search = bridge.call_tool("search_images", json!({ "pageSize": 5, "view": "all" }))?;
    let tags = bridge.call_tool("tags_list", json!({ "limit": 10 }))?;
    drop(bridge);

    let state = app.state::<AppState>();
    set_mcp(app, request.port, McpMode::Full, false)?;
    wait_stopped(state.inner(), Duration::from_secs(5))?;
    let runtime_removed = !paths::mcp_runtime_path().is_file();
    Ok(json!({
        "initialize": initialize,
        "toolCount": tools["result"]["tools"].as_array().map_or(0, Vec::len),
        "libraryInfo": info.to_value(),
        "search": search.to_value(),
        "tags": tags.to_value(),
        "runtimeRemovedAfterStop": runtime_removed,
    }))
}

pub(super) fn scenario_modes(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let request: ModesRequest = options.request()?;
    if request.port == 0 {
        return Err("场景需要显式 port".into());
    }
    if request.image_ids.is_empty() {
        return Err("场景需要 imageIds".into());
    }
    let ids = request.image_ids.clone();
    let mut bridge = Bridge::start_after_start(app, request.port)?;
    let mut output = serde_json::Map::new();

    // 只读档：写工具仍在列表里，但调用被明确拒绝。
    set_mcp(app, request.port, McpMode::ReadOnly, true)?;
    output.insert(
        "readOnly".into(),
        json!({
            "libraryInfo": bridge.call_tool("library_info", json!({}))?.to_value(),
            "update": bridge.call_tool("images_update", json!({
                "selector": { "kind": "ids", "ids": ids },
                "score": 5,
                "confirmedCount": ids.len(),
            }))?.to_value(),
        }),
    );

    // 询问档：移入回收站必须确认；拒绝、批准、过期各走一次。
    set_mcp(app, request.port, McpMode::Ask, true)?;
    let dry_run = bridge.call_tool(
        "images_trash",
        json!({
            "selector": { "kind": "ids", "ids": ids },
            "dryRun": true,
        }),
    )?;
    let pending = bridge.call_tool(
        "images_trash",
        json!({
            "selector": { "kind": "ids", "ids": ids },
            "confirmedCount": dry_run.data["selected"],
        }),
    )?;
    let pending_id = pending.data["approvalId"]
        .as_str()
        .ok_or_else(|| format!("询问档没有返回 approvalId：{}", pending.text))?;
    let state = app.state::<AppState>();
    state
        .reject_mcp_approval(pending_id)
        .map_err(|error| error.to_string())?;
    let rejected = approval_view(state.inner(), pending_id);

    let pending = bridge.call_tool(
        "images_trash",
        json!({
            "selector": { "kind": "ids", "ids": ids },
            "confirmedCount": dry_run.data["selected"],
        }),
    )?;
    let approved_id = pending.data["approvalId"]
        .as_str()
        .ok_or_else(|| format!("询问档没有返回 approvalId：{}", pending.text))?;
    let approved = state
        .approve_mcp_approval(approved_id)
        .map_err(|error| error.to_string())?;
    let counts_after_approve = counts(state.inner())?;
    state
        .restore_images(&ids)
        .map_err(|error| error.to_string())?;
    let approval_status = approval_view(state.inner(), approved_id);

    // 过期：无头构建把有效期缩到秒级，避免等 10 分钟。
    let pending = bridge.call_tool(
        "images_trash",
        json!({
            "selector": { "kind": "ids", "ids": ids },
            "confirmedCount": ids.len(),
        }),
    )?;
    let expired_id = pending.data["approvalId"]
        .as_str()
        .ok_or_else(|| format!("询问档没有返回 approvalId：{}", pending.text))?
        .to_owned();
    std::thread::sleep(crate::mcp::approval::ttl() + Duration::from_millis(250));
    let expired_status = approval_view(state.inner(), &expired_id);
    let expired_error = state
        .approve_mcp_approval(&expired_id)
        .err()
        .map(|error| error.to_string());

    // 换库：旧确认单不能带到新库。
    let pending = bridge.call_tool(
        "images_trash",
        json!({
            "selector": { "kind": "ids", "ids": ids },
            "confirmedCount": ids.len(),
        }),
    )?;
    pending.data["approvalId"]
        .as_str()
        .ok_or_else(|| format!("询问档没有返回 approvalId：{}", pending.text))?;
    let library_path = state
        .inner()
        .status()
        .map_err(|error| error.to_string())?
        .map(|info| info.path)
        .ok_or_else(|| "换库前图库未打开".to_owned())?;
    state
        .inner()
        .close_library()
        .map_err(|error| error.to_string())?;
    state
        .inner()
        .open_library(Path::new(&library_path))
        .map_err(|error| error.to_string())?;
    let cleared = state.inner().mcp_approval_views();

    // 完全档：不弹确认，阈值返回 null。
    set_mcp(app, request.port, McpMode::Full, true)?;
    let full_info = bridge.call_tool("library_info", json!({}))?;
    let full_dry = bridge.call_tool(
        "images_trash",
        json!({
            "selector": { "kind": "ids", "ids": ids },
            "dryRun": true,
        }),
    )?;
    let full_trash = bridge.call_tool(
        "images_trash",
        json!({
            "selector": { "kind": "ids", "ids": ids },
            "confirmedCount": full_dry.data["selected"],
        }),
    )?;
    let counts_after_full = counts(state.inner())?;
    state
        .inner()
        .restore_images(&ids)
        .map_err(|error| error.to_string())?;

    // 生命周期：停服删运行时文件，重新开启换新口令；停服后小桥给出可读错误。
    let token_before = read_runtime_token()?;
    set_mcp(app, request.port, McpMode::Full, false)?;
    wait_stopped(state.inner(), Duration::from_secs(5))?;
    let runtime_removed = !paths::mcp_runtime_path().is_file();
    let stopped_call = bridge
        .call_tool("library_info", json!({}))
        .map(|reply| reply.to_value())
        .unwrap_or_else(|error| json!({ "error": error }));
    set_mcp(app, request.port, McpMode::Full, true)?;
    wait_running(app, Duration::from_secs(10))?;
    let token_after = read_runtime_token()?;

    output.insert(
        "ask".into(),
        json!({
            "dryRun": dry_run.to_value(),
            "rejectedStatus": rejected,
            "approved": approved,
            "approvalStatus": approval_status,
            "countsAfterApprove": counts_after_approve,
            "expiredStatus": expired_status,
            "expiredError": expired_error,
            "approvalsAfterSwitch": cleared.len(),
        }),
    );
    output.insert(
        "full".into(),
        json!({
            "libraryInfo": full_info.to_value(),
            "trash": full_trash.to_value(),
            "countsAfterTrash": counts_after_full,
        }),
    );
    output.insert(
        "lifecycle".into(),
        json!({
            "tokenBefore": token_before,
            "tokenAfter": token_after,
            "tokenChanged": token_before != token_after,
            "runtimeRemovedAfterStop": runtime_removed,
            "callAfterStop": stopped_call,
        }),
    );
    Ok(Value::Object(output))
}

fn set_mcp(app: &tauri::AppHandle, port: u16, mode: McpMode, enabled: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut settings = state.settings.snapshot();
    settings.mcp = McpSettings {
        enabled,
        port,
        mode,
    };
    super::save_settings(state.inner(), settings).map(|_| ())
}

fn wait_running(app: &tauri::AppHandle, timeout: Duration) -> Result<McpStatus, String> {
    let state = app.state::<AppState>();
    wait_status(&state, timeout, |status| status.running)
}

fn wait_stopped(state: &AppState, timeout: Duration) -> Result<McpStatus, String> {
    wait_status(state, timeout, |status| !status.running)
}

fn wait_status(
    state: &AppState,
    timeout: Duration,
    ready: impl Fn(&McpStatus) -> bool,
) -> Result<McpStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = state.mcp_status();
        if ready(&status) {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(format!("MCP 状态没有在时限内就绪：{status:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn approval_view(state: &AppState, id: &str) -> Value {
    state
        .mcp_approval_views()
        .into_iter()
        .find(|view| view.id == id)
        .map(|view| serde_json::to_value(view).unwrap_or(Value::Null))
        .unwrap_or(Value::Null)
}

fn counts(state: &AppState) -> Result<Value, String> {
    let counts = AppError::command(state.counts()).map_err(|error| error.to_string())?;
    serde_json::to_value(counts).map_err(|error| format!("无法序列化计数：{error}"))
}

fn read_runtime_token() -> Result<String, String> {
    let path = paths::mcp_runtime_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("读取 MCP 运行时文件失败 {}：{error}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("MCP 运行时文件不是有效 JSON：{error}"))?;
    value["token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "MCP 运行时文件缺少 token".to_owned())
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": "2026-07-28",
        "capabilities": {},
        "clientInfo": { "name": "acceptance", "version": "1" },
    })
}

struct ToolReply {
    text: String,
    data: Value,
}

impl ToolReply {
    fn to_value(&self) -> Value {
        self.data.clone()
    }
}

struct Bridge {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: i64,
}

impl Bridge {
    fn start_after_start(app: &tauri::AppHandle, port: u16) -> Result<Self, String> {
        let mut bridge = Self::start()?;
        set_mcp(app, port, McpMode::ReadOnly, true)?;
        wait_running(app, Duration::from_secs(10))?;
        bridge.request("initialize", initialize_params())?;
        Ok(bridge)
    }

    fn start() -> Result<Self, String> {
        let exe =
            std::env::current_exe().map_err(|error| format!("无法取得当前可执行文件：{error}"))?;
        let mut child = Command::new(&exe)
            .arg("--mcp-stdio")
            .env("LILITH_ARTBANK_ROOT", paths::app_root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("无法启动 MCP 小桥：{error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "小桥没有 stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "小桥没有 stdout".to_owned())?;
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            next_id: 1,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{payload}")
            .and_then(|()| self.stdin.flush())
            .map_err(|error| format!("写入 MCP 请求失败：{error}"))?;
        loop {
            let line =
                self.lines
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|error| match error {
                        RecvTimeoutError::Timeout => format!("等待 MCP 响应超时：{method}"),
                        RecvTimeoutError::Disconnected => "MCP 小桥已退出".to_owned(),
                    })?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)
                .map_err(|error| format!("MCP 响应不是有效 JSON：{error}（{line}）"))?;
            if value["id"] == id || value["id"].is_null() {
                if let Some(error) = value.get("error") {
                    return Err(error["message"]
                        .as_str()
                        .unwrap_or("MCP 返回错误")
                        .to_owned());
                }
                return Ok(value);
            }
        }
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<ToolReply, String> {
        let response = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )?;
        let result = &response["result"];
        let text = result["content"]
            .as_array()
            .and_then(|content| content.first())
            .and_then(|block| block["text"].as_str())
            .unwrap_or_default()
            .to_owned();
        let data = serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()));
        Ok(ToolReply { text, data })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeRequest {
    port: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModesRequest {
    port: u16,
    image_ids: Vec<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_reply_parses_json_and_keeps_plain_text() {
        let reply = ToolReply {
            text: r#"{"status":"rejected","reason":"no"}"#.into(),
            data: json!({ "status": "rejected", "reason": "no" }),
        };
        assert_eq!(reply.to_value()["status"], "rejected");
        let plain = ToolReply {
            text: "当前是只读模式".into(),
            data: Value::String("当前是只读模式".into()),
        };
        assert_eq!(plain.to_value(), "当前是只读模式");
    }
}

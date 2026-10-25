use std::{
    convert::Infallible,
    net::TcpListener,
    sync::{
        Arc,
        mpsc::{Receiver, RecvTimeoutError, channel},
    },
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Response, StatusCode,
    header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tokio_util::sync::CancellationToken;
use tower::Service;

use crate::{
    app::{
        error::{AppError, AppResult},
        paths,
        settings::{McpMode, McpSettings},
    },
    mcp::server::McpServer,
};

const RUNTIME_VERSION: u32 = 1;
const MAX_REQUEST_BODY: usize = 1024 * 1024;
/// 停止服务时等待监听套接字关闭的上限。
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeFile {
    pub version: u32,
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub started_at: i64,
    pub mode: McpMode,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    pub enabled: bool,
    pub running: bool,
    pub port: u16,
    pub mode: McpMode,
    pub runtime_path: String,
    pub error: Option<String>,
}

struct Running {
    port: u16,
    cancel: CancellationToken,
    /// 服务任务退出时收到一次通知；`stop()` 用它等端口释放。
    stopped: Receiver<()>,
}

struct Inner {
    running: Mutex<Option<Running>>,
    last_error: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct McpRuntime {
    inner: Arc<Inner>,
}

impl Default for McpRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl McpRuntime {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                running: Mutex::new(None),
                last_error: Mutex::new(None),
            }),
        }
    }

    /// 设置保存或启动时调用：先停旧服务，再按新设置决定是否启动。
    ///
    /// 端口被占用等启动失败只记录在状态里，不影响设置保存。
    pub fn apply(&self, app: AppHandle, settings: &McpSettings) {
        self.stop();
        if !settings.enabled {
            *self.inner.last_error.lock() = None;
            return;
        }
        match self.start(app, settings) {
            Ok(()) => {
                *self.inner.last_error.lock() = None;
                log::info!("MCP 服务已启动：127.0.0.1:{}", settings.port);
            }
            Err(error) => {
                log::warn!("MCP 服务启动失败：{error}");
                *self.inner.last_error.lock() = Some(error.to_string());
            }
        }
    }

    fn start(&self, app: AppHandle, settings: &McpSettings) -> AppResult<()> {
        if self.inner.running.lock().is_some() {
            return Err(AppError::message("MCP 服务已经在运行"));
        }
        let listener = TcpListener::bind(("127.0.0.1", settings.port)).map_err(|error| {
            AppError::message(format!(
                "MCP 端口 {} 无法监听（{error}），请换一个端口",
                settings.port
            ))
        })?;

        let token = new_token();
        let runtime_path = paths::mcp_runtime_path();
        let runtime = RuntimeFile {
            version: RUNTIME_VERSION,
            port: settings.port,
            token: token.clone(),
            pid: std::process::id(),
            started_at: chrono::Local::now().timestamp_millis(),
            mode: settings.mode,
        };
        write_runtime(&runtime_path, &runtime)?;

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_token = token.clone();
        let (stopped_tx, stopped) = channel();
        tauri::async_runtime::spawn(async move {
            if let Err(error) = serve(listener, task_token, app, task_cancel).await {
                log::warn!("MCP 服务停止：{error}");
            }
            // 监听套接字随 serve 返回被关闭，这里通知 `stop()` 可以放行了。
            let _ = stopped_tx.send(());
        });
        *self.inner.running.lock() = Some(Running {
            port: settings.port,
            cancel,
            stopped,
        });
        Ok(())
    }

    /// 停止服务并删除运行时文件；最多阻塞 [`STOP_TIMEOUT`]，等监听端口真正释放。
    ///
    /// Windows 下同一端口不能重复 bind，所以保存设置后立刻按新配置重启时，
    /// 必须等旧监听套接字关闭，否则会误报“端口无法监听”。
    pub fn stop(&self) {
        let Some(running) = self.inner.running.lock().take() else {
            remove_runtime_file();
            return;
        };
        running.cancel.cancel();
        remove_runtime_file();
        if let Err(RecvTimeoutError::Timeout) = running.stopped.recv_timeout(STOP_TIMEOUT) {
            log::warn!(
                "MCP 服务没有在 {} 毫秒内停止（端口 {}）",
                STOP_TIMEOUT.as_millis(),
                running.port
            );
        }
        log::info!("MCP 服务已停止（端口 {}）", running.port);
    }

    pub fn status(&self, settings: &McpSettings) -> McpStatus {
        let running = self.inner.running.lock();
        McpStatus {
            enabled: settings.enabled,
            running: running.is_some(),
            port: running
                .as_ref()
                .map(|running| running.port)
                .unwrap_or(settings.port),
            mode: settings.mode,
            runtime_path: paths::mcp_runtime_path().to_string_lossy().into_owned(),
            error: self.inner.last_error.lock().clone(),
        }
    }
}

async fn serve(
    listener: TcpListener,
    token: String,
    app: AppHandle,
    cancel: CancellationToken,
) -> AppResult<()> {
    serve_with(
        listener,
        token,
        move || Ok(McpServer::new(app.clone())),
        cancel,
    )
    .await
}

async fn serve_with<S, F>(
    listener: TcpListener,
    token: String,
    service_factory: F,
    cancel: CancellationToken,
) -> AppResult<()>
where
    S: rmcp::ServerHandler + Send + 'static,
    F: Fn() -> Result<S, std::io::Error> + Send + Sync + 'static,
{
    listener
        .set_nonblocking(true)
        .map_err(|error| AppError::message(format!("MCP 监听设置失败：{error}")))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|error| AppError::message(format!("MCP 监听初始化失败：{error}")))?;
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(cancel.clone())
        .with_max_request_body_bytes(MAX_REQUEST_BODY)
        .with_sse_keep_alive(None)
        .with_sse_retry(None);
    let service = StreamableHttpService::new(
        service_factory,
        Arc::new(NeverSessionManager::default()),
        config,
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(value) => value,
                    Err(error) => {
                        if cancel.is_cancelled() {
                            break;
                        }
                        log::warn!("MCP 连接接收失败：{error}");
                        continue;
                    }
                };
                let service = service.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let connection = service_fn(move |request| {
                        let mut service = service.clone();
                        let token = token.clone();
                        async move {
                            if !authorized(request.headers(), &token) {
                                return Ok::<_, Infallible>(unauthorized_response());
                            }
                            Service::call(&mut service, request).await
                        }
                    });
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, connection)
                        .await
                    {
                        log::debug!("MCP HTTP 连接结束：{error}");
                    }
                });
            }
        }
    }
    Ok(())
}

fn authorized(headers: &hyper::HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(candidate) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(candidate.trim().as_bytes(), token.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn unauthorized_response() -> Response<http_body_util::combinators::BoxBody<Bytes, Infallible>> {
    let body =
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"MCP 口令无效或缺失"}}"#;
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(CONTENT_TYPE, "application/json")
        .header(CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from_static(body.as_bytes())).boxed())
        .expect("unauthorized response is valid")
}

fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn write_runtime(path: &std::path::Path, runtime: &RuntimeFile) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(runtime)?;
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn remove_runtime_file() {
    let path = paths::mcp_runtime_path();
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => log::warn!("删除 MCP 运行时文件失败（{}）：{error}", path.display()),
    }
}

/// stdio 小桥读运行时文件拿端口与口令；应用未启动或服务未开启时返回可读错误。
pub fn read_runtime() -> AppResult<RuntimeFile> {
    let path = paths::mcp_runtime_path();
    let bytes = std::fs::read(&path).map_err(|error| {
        AppError::message(format!(
            "找不到 MCP 运行时文件（{}）：应用未启动或 MCP 服务未开启（{error}）",
            path.display()
        ))
    })?;
    let runtime: RuntimeFile = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::message(format!("MCP 运行时文件格式无效：{error}")))?;
    if runtime.version != RUNTIME_VERSION {
        return Err(AppError::message("MCP 运行时文件版本不受支持"));
    }
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rmcp::{
        ServerHandler,
        handler::server::router::tool::ToolRouter,
        model::{ServerCapabilities, ServerConfig},
        tool, tool_handler, tool_router,
    };
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct EchoServer {
        tool_router: ToolRouter<Self>,
    }

    impl EchoServer {
        fn new() -> Self {
            Self {
                tool_router: Self::tool_router(),
            }
        }
    }

    #[tool_router]
    impl EchoServer {
        #[tool(description = "echo")]
        fn echo(&self) -> String {
            "pong".to_owned()
        }
    }

    #[tool_handler(router = self.tool_router)]
    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }
    }

    #[test]
    fn compares_tokens_without_length_leaks() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn parses_runtime_file_and_rejects_bad_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp-runtime.json");
        let runtime = RuntimeFile {
            version: RUNTIME_VERSION,
            port: 18767,
            token: "token".into(),
            pid: 1,
            started_at: 0,
            mode: McpMode::Ask,
        };
        write_runtime(&path, &runtime).unwrap();
        let loaded: RuntimeFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded.port, 18767);
        assert_eq!(loaded.mode, McpMode::Ask);

        std::fs::write(
            &path,
            br#"{"version":99,"port":1,"token":"x","pid":1,"startedAt":0,"mode":"ask"}"#,
        )
        .unwrap();
        let loaded: RuntimeFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_ne!(loaded.version, RUNTIME_VERSION);
    }

    #[test]
    fn serves_stateless_json_and_requires_bearer_token() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let _ = runtime.block_on(serve_with(
                listener,
                "secret-token".to_owned(),
                || Ok(EchoServer::new()),
                task_cancel,
            ));
        });

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build();
        let url = format!("http://127.0.0.1:{port}/mcp");
        let unauthorized = agent
            .post(&url)
            .set("Authorization", "Bearer wrong")
            .send_json(json!({}))
            .unwrap_err();
        assert!(matches!(unauthorized, ureq::Error::Status(401, _)));

        let initialize = agent
            .post(&url)
            .set("Authorization", "Bearer secret-token")
            .set("Accept", "application/json, text/event-stream")
            .send_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1" }
                }
            }))
            .unwrap()
            .into_string()
            .unwrap();
        let initialized: serde_json::Value = serde_json::from_str(&initialize).unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");

        let list = agent
            .post(&url)
            .set("Authorization", "Bearer secret-token")
            .set("Accept", "application/json, text/event-stream")
            .set("MCP-Protocol-Version", "2025-06-18")
            .send_json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }))
            .unwrap()
            .into_string()
            .unwrap();
        let listed: serde_json::Value = serde_json::from_str(&list).unwrap();
        assert_eq!(listed["result"]["tools"][0]["name"], "echo");

        cancel.cancel();
        server.join().unwrap();
    }

    #[test]
    fn requires_bearer_authorization_header() {
        let mut headers = hyper::HeaderMap::new();
        assert!(!authorized(&headers, "secret"));
        headers.insert(AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(authorized(&headers, "secret"));
        headers.insert(AUTHORIZATION, "Bearer other".parse().unwrap());
        assert!(!authorized(&headers, "secret"));
    }

    /// `stop()` 靠“服务任务退出”这一信号判断端口已释放；Windows 不允许同端口重复 bind，
    /// 所以这条顺序必须成立，保存设置后才能立刻按新配置重启。
    #[test]
    fn closed_server_releases_the_port_for_restart() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let (stopped_tx, stopped) = channel();
        let task_cancel = cancel.clone();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let _ = runtime.block_on(serve_with(
                listener,
                "secret-token".to_owned(),
                || Ok(EchoServer::new()),
                task_cancel,
            ));
            let _ = stopped_tx.send(());
        });

        // 等 accept 循环真正跑起来，避免取消早于监听进入 select。
        std::thread::sleep(Duration::from_millis(100));
        cancel.cancel();
        stopped
            .recv_timeout(Duration::from_secs(5))
            .expect("服务任务应在取消后退出");
        server.join().unwrap();

        let rebound =
            TcpListener::bind(("127.0.0.1", port)).expect("停止后应能立刻重新监听同一端口");
        drop(rebound);
    }
}

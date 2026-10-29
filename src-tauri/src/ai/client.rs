use std::{
    collections::HashMap,
    fmt,
    io::Read,
    sync::{
        Arc,
        mpsc::{RecvTimeoutError, channel},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::library::viewer::{PoseData, PoseLandmark};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
const ENSURE_TIMEOUT: Duration = Duration::from_secs(600);
/// 数据请求（embedding、pose、描述）单次尝试上限：服务卡死时不再一等等 5 分钟。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// 数据请求的总预算：重试也算在里面，避免「300 秒 × 3 次」把取消拖成十几分钟。
const REQUEST_BUDGET: Duration = Duration::from_secs(300);
const MAX_ATTEMPTS: usize = 3;
/// 模型加载可能确实要几分钟，单独给一份更宽的预算。
const ENSURE_BUDGET: Duration = Duration::from_secs(660);
const ENSURE_ATTEMPTS: usize = 2;
/// 重试退避；取消能打断等待，所以按小片睡。
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];
/// 等待响应与退避时多久看一次取消标记。
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// 取消信号：任务取消后为 true。批量任务把它传给客户端，取消能立刻生效。
pub type CancelSignal = Arc<dyn Fn() -> bool + Send + Sync>;

/// `/v1` 调用失败。`retryable` 决定是否再试：网络错误与 5xx 为 true，4xx、超时与响应格式错误为 false。
#[derive(Debug, Clone)]
pub struct AiError {
    pub message: String,
    pub retryable: bool,
    /// 连接层失败（进程未启动、端口未监听等）；面向外部的文案不应带出内部地址与 OS 错误。
    pub transport: bool,
    /// 已被打断（用户取消或超出总预算）：`with_budget` 据此不再重试。
    /// 它不等于「用户点了取消」——判断后者要看任务自己的取消标记。
    pub cancelled: bool,
}

impl AiError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
            transport: true,
            cancelled: false,
        }
    }

    fn status(code: u16, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: (500..=599).contains(&code),
            transport: false,
            cancelled: false,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            transport: false,
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            message: "AI 请求已取消".to_owned(),
            retryable: false,
            transport: false,
            cancelled: true,
        }
    }

    fn budget_exhausted() -> Self {
        Self {
            message: "AI 服务在总时限内没有响应，已停止等待".to_owned(),
            retryable: false,
            transport: false,
            cancelled: true,
        }
    }
}

impl fmt::Display for AiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AiError {}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capability {
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub model: String,
    pub dim: Option<u32>,
}

/// `/v1/health` 与 `/v1/models/ensure` 的公共结构；没有该能力时字段可整体缺省。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    #[serde(default)]
    pub status: String,
    pub version: Option<String>,
    #[serde(default)]
    pub capabilities: HashMap<String, Capability>,
}

impl Health {
    pub fn capability(&self, name: &str) -> Option<&Capability> {
        self.capabilities.get(name)
    }
}

#[derive(Debug, Clone)]
pub struct Embedding {
    pub model: String,
    pub dim: u32,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct Description {
    pub model: String,
    pub text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmbeddingResponse {
    status: Option<String>,
    model: String,
    dim: u32,
    vector: Vec<f32>,
    /// 文本 embedding 被服务端截断时置位；客户端只记日志，不改变结果。
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLandmark {
    x: f64,
    y: f64,
    z: f64,
    visibility: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PoseResponse {
    status: Option<String>,
    model: String,
    landmarks: Vec<RawLandmark>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DescribeResponse {
    status: Option<String>,
    model: String,
    text: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct CapabilitiesResponse {
    #[serde(default)]
    capabilities: HashMap<String, Capability>,
}

#[derive(Debug, Clone, Copy)]
enum Method {
    Get,
    Post,
}

pub struct AiClient {
    base_url: String,
    agent: ureq::Agent,
    cancel: Option<CancelSignal>,
    /// 单次尝试上限与总预算；正常构建用常量，测试用小值避免真等 5 分钟。
    attempt_timeout: Duration,
    budget: Duration,
}

impl AiClient {
    /// `base_url` 去掉首尾空白与结尾斜杠；所有请求共用同一个 agent（连接复用）。
    pub fn new(base_url: &str) -> Self {
        Self::build(base_url, None)
    }

    /// 带取消信号的客户端：批量任务用它，取消时正在飞的请求也能立刻返回。
    pub fn cancellable(base_url: &str, cancel: CancelSignal) -> Self {
        Self::build(base_url, Some(cancel))
    }

    fn build(base_url: &str, cancel: Option<CancelSignal>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .build();
        Self {
            base_url: base_url.trim().trim_end_matches('/').to_owned(),
            agent,
            cancel,
            attempt_timeout: REQUEST_TIMEOUT,
            budget: REQUEST_BUDGET,
        }
    }

    #[cfg(test)]
    fn with_limits(mut self, attempt_timeout: Duration, budget: Duration) -> Self {
        self.attempt_timeout = attempt_timeout;
        self.budget = budget;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    pub fn health(&self) -> Result<Health, AiError> {
        let body = self.execute(
            Method::Get,
            &self.url("/v1/health"),
            None,
            None,
            HEALTH_TIMEOUT,
        )?;
        parse_body(&body)
    }

    pub fn ensure_models(
        &self,
        capabilities: &[String],
    ) -> Result<HashMap<String, Capability>, AiError> {
        let payload = serde_json::json!({ "capabilities": capabilities }).to_string();
        let url = self.url("/v1/models/ensure");
        self.with_budget(ENSURE_TIMEOUT, ENSURE_ATTEMPTS, ENSURE_BUDGET, |timeout| {
            let body = self.execute(
                Method::Post,
                &url,
                Some("application/json; charset=utf-8"),
                Some(payload.as_bytes()),
                timeout,
            )?;
            let parsed: CapabilitiesResponse = parse_body(&body)?;
            Ok(parsed.capabilities)
        })
    }

    pub fn embed_image(&self, model: &str, bytes: &[u8]) -> Result<Embedding, AiError> {
        let url = query_url(&self.url("/v1/embed/image"), &[("model", model)]);
        self.with_budget(self.attempt_timeout, MAX_ATTEMPTS, self.budget, |timeout| {
            let body = self.execute(
                Method::Post,
                &url,
                Some("application/octet-stream"),
                Some(bytes),
                timeout,
            )?;
            let parsed: EmbeddingResponse = parse_body(&body)?;
            embedding_from(parsed)
        })
    }

    pub fn embed_text(&self, model: &str, text: &str) -> Result<Embedding, AiError> {
        let payload = serde_json::json!({ "text": text, "model": model }).to_string();
        let url = self.url("/v1/embed/text");
        self.with_budget(self.attempt_timeout, MAX_ATTEMPTS, self.budget, |timeout| {
            let body = self.execute(
                Method::Post,
                &url,
                Some("application/json; charset=utf-8"),
                Some(payload.as_bytes()),
                timeout,
            )?;
            let parsed: EmbeddingResponse = parse_body(&body)?;
            embedding_from(parsed)
        })
    }

    pub fn pose(&self, model: &str, bytes: &[u8]) -> Result<PoseData, AiError> {
        let url = query_url(&self.url("/v1/pose"), &[("model", model)]);
        self.with_budget(self.attempt_timeout, MAX_ATTEMPTS, self.budget, |timeout| {
            let body = self.execute(
                Method::Post,
                &url,
                Some("application/octet-stream"),
                Some(bytes),
                timeout,
            )?;
            let parsed: PoseResponse = parse_body(&body)?;
            if parsed
                .status
                .as_deref()
                .is_some_and(|value| value != "success")
            {
                return Err(AiError::invalid("姿势服务返回失败状态"));
            }
            if parsed.landmarks.len() > 33 {
                return Err(AiError::invalid(format!(
                    "姿势服务返回了 {} 个点，MediaPipe Pose 应为 33 个",
                    parsed.landmarks.len()
                )));
            }
            let landmarks = parsed
                .landmarks
                .into_iter()
                .map(|point| PoseLandmark {
                    x: finite(point.x).unwrap_or(0.0) as f32,
                    y: finite(point.y).unwrap_or(0.0) as f32,
                    z: finite(point.z).unwrap_or(0.0) as f32,
                    visibility: finite(point.visibility).unwrap_or(0.0).clamp(0.0, 1.0) as f32,
                })
                .collect();
            Ok(PoseData {
                model: parsed.model,
                landmarks,
            })
        })
    }

    pub fn describe(
        &self,
        prompt: &str,
        max_tokens: Option<u32>,
        bytes: &[u8],
    ) -> Result<Description, AiError> {
        let max_tokens_text = max_tokens.map(|value| value.to_string());
        let mut query = vec![("prompt", prompt)];
        if let Some(value) = max_tokens_text.as_deref() {
            query.push(("maxTokens", value));
        }
        let url = query_url(&self.url("/v1/describe"), &query);
        self.with_budget(self.attempt_timeout, MAX_ATTEMPTS, self.budget, |timeout| {
            let body = self.execute(
                Method::Post,
                &url,
                Some("application/octet-stream"),
                Some(bytes),
                timeout,
            )?;
            let parsed: DescribeResponse = parse_body(&body)?;
            if parsed
                .status
                .as_deref()
                .is_some_and(|value| value != "success")
            {
                return Err(AiError::invalid("描述服务返回失败状态"));
            }
            Ok(Description {
                model: parsed.model,
                text: parsed.text.trim().to_owned(),
            })
        })
    }

    pub fn shutdown(&self) -> Result<(), AiError> {
        self.execute(
            Method::Post,
            &self.url("/v1/shutdown"),
            None,
            Some(b""),
            Duration::from_secs(2),
        )
        .map(|_| ())
    }

    /// 一次逻辑调用：按预算切成若干次尝试，每次都有独立超时；取消在请求进行中也能立刻生效。
    fn with_budget<T>(
        &self,
        attempt_timeout: Duration,
        attempts: usize,
        budget: Duration,
        mut call: impl FnMut(Duration) -> Result<T, AiError>,
    ) -> Result<T, AiError> {
        let attempts = attempts.max(1);
        let deadline = Instant::now() + budget;
        for attempt in 0..attempts {
            if attempt > 0 {
                let delay = RETRY_DELAYS[(attempt - 1).min(RETRY_DELAYS.len() - 1)];
                self.sleep_unless_cancelled(delay)?;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AiError::budget_exhausted());
            }
            match call(remaining.min(attempt_timeout)) {
                Ok(value) => return Ok(value),
                Err(error) if error.cancelled || !error.retryable => return Err(error),
                Err(error) if attempt + 1 == attempts => return Err(error),
                Err(_) => {}
            }
        }
        unreachable!("尝试次数已收敛到至少 1，循环一定返回")
    }

    /// 退避等待按小片睡，取消能立刻打断。
    fn sleep_unless_cancelled(&self, delay: Duration) -> Result<(), AiError> {
        let deadline = Instant::now() + delay;
        loop {
            if self.is_cancelled() {
                return Err(AiError::cancelled());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            std::thread::sleep(remaining.min(CANCEL_POLL));
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|cancel| cancel())
    }

    /// 发一次请求并读回响应体。
    ///
    /// 阻塞客户端没有中断接口，所以带取消信号时把请求放到旁路线程，这里每 100ms 看一次取消：
    /// 取消后立即返回，旁路线程受单次超时约束，最多再跑完这一次尝试就自行结束。
    fn execute(
        &self,
        method: Method,
        url: &str,
        content_type: Option<&'static str>,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<String, AiError> {
        if self.is_cancelled() {
            return Err(AiError::cancelled());
        }
        let Some(cancel) = self.cancel.clone() else {
            return send(&self.agent, method, url, content_type, body, timeout);
        };
        let agent = self.agent.clone();
        let owned_url = url.to_owned();
        let owned = body.map(<[u8]>::to_vec);
        let (sender, receiver) = channel();
        std::thread::Builder::new()
            .name("ai-request".to_owned())
            .spawn(move || {
                let _ = sender.send(send(
                    &agent,
                    method,
                    &owned_url,
                    content_type,
                    owned.as_deref(),
                    timeout,
                ));
            })
            .map_err(|error| AiError::transport(format!("无法启动 AI 请求线程：{error}")))?;
        loop {
            if cancel() {
                return Err(AiError::cancelled());
            }
            match receiver.recv_timeout(CANCEL_POLL) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AiError::transport("AI 请求线程异常退出"));
                }
            }
        }
    }
}

/// 真正发出请求；超时、状态码与响应体错误都在这里转成 `AiError`。
fn send(
    agent: &ureq::Agent,
    method: Method,
    url: &str,
    content_type: Option<&'static str>,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<String, AiError> {
    let request = match method {
        Method::Get => agent.get(url),
        Method::Post => agent.post(url),
    }
    .timeout(timeout);
    let request = match content_type {
        Some(value) => request.set("Content-Type", value),
        None => request,
    };
    let response = match body {
        Some(bytes) => request.send_bytes(bytes),
        None => request.call(),
    }
    .map_err(map_error)?;
    response
        .into_string()
        .map_err(|error| AiError::transport(format!("读取 AI 服务响应失败：{error}")))
}

fn embedding_from(response: EmbeddingResponse) -> Result<Embedding, AiError> {
    if response
        .status
        .as_deref()
        .is_some_and(|value| value != "success")
    {
        return Err(AiError::invalid("embedding 服务返回失败状态"));
    }
    if response.dim == 0 || response.vector.len() != response.dim as usize {
        return Err(AiError::invalid(format!(
            "embedding 维度不匹配：声明 {}，返回 {}",
            response.dim,
            response.vector.len()
        )));
    }
    if response.truncated {
        log::warn!("文本 embedding 被服务端截断，按截断后的向量继续");
    }
    // 服务端通常已归一化，这里再归一化一次，避免不守约的服务端污染检索。
    let vector = normalize(response.vector)?;
    Ok(Embedding {
        model: response.model,
        dim: response.dim,
        vector,
    })
}

pub fn normalize(vector: Vec<f32>) -> Result<Vec<f32>, AiError> {
    super::embedding::normalize_f32(&vector).map_err(|error| AiError::invalid(error.to_string()))
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn parse_body<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, AiError> {
    serde_json::from_str(body)
        .map_err(|error| AiError::invalid(format!("AI 服务响应格式错误：{error}")))
}

fn map_error(error: ureq::Error) -> AiError {
    match error {
        ureq::Error::Status(code, response) => {
            let mut body = String::new();
            let _ = response
                .into_reader()
                .take(64 * 1024)
                .read_to_string(&mut body);
            let message = serde_json::from_str::<ErrorBody>(&body)
                .ok()
                .and_then(|body| body.message.or(body.error))
                .filter(|message| !message.trim().is_empty())
                .unwrap_or_else(|| format!("AI 服务返回 HTTP {code}"));
            AiError::status(code, message)
        }
        ureq::Error::Transport(error) => AiError::transport(format!("无法连接 AI 服务：{error}")),
    }
}

fn query_url(base: &str, params: &[(&str, &str)]) -> String {
    let params: Vec<String> = params
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect();
    if params.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", params.join("&"))
    }
}

/// 最小 RFC 3986 百分号编码；查询值只有模型名与提示词，不需要处理完整 URI。
fn encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                output.push(byte as char);
            }
            _ => {
                output.push('%');
                output.push(HEX[usize::from(byte >> 4)] as char);
                output.push(HEX[usize::from(byte & 0x0F)] as char);
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::test_support::{MockResponse, MockServer};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn capability_json() -> String {
        serde_json::json!({
            "status": "ok",
            "version": "1",
            "capabilities": {
                "imageEmbedding": {"ready": true, "model": "siglip2-test", "dim": 4},
                "textEmbedding": {"ready": true, "model": "siglip2-test", "dim": 4},
                "pose": {"ready": true, "model": "mediapipe-pose"},
                "description": {"ready": true, "model": "desc-test"}
            }
        })
        .to_string()
    }

    #[test]
    fn parses_health_and_normalizes_embedding() {
        let server = MockServer::start(|request, _| {
            if request.path.starts_with("/v1/health") {
                MockResponse::json(200, capability_json())
            } else if request.path.starts_with("/v1/embed/image") {
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "status": "success",
                        "model": "siglip2-test",
                        "dim": 4,
                        "normalized": true,
                        "vector": [3.0, 4.0, 0.0, 0.0]
                    })
                    .to_string(),
                )
            } else {
                MockResponse::json(404, "{}")
            }
        });
        let client = AiClient::new(&server.url());
        let health = client.health().unwrap();
        let capability = health.capability("imageEmbedding").unwrap();
        assert!(capability.ready);
        assert_eq!(capability.dim, Some(4));

        let embedding = client.embed_image("siglip2-test", b"image-bytes").unwrap();
        assert_eq!(embedding.model, "siglip2-test");
        assert_eq!(embedding.dim, 4);
        assert!((embedding.vector[0] - 0.6).abs() < 1e-6);
        assert!((embedding.vector[1] - 0.8).abs() < 1e-6);
        let requests = server.requests();
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].body, b"image-bytes");
        assert!(requests[1].path.contains("model=siglip2-test"));
    }

    #[test]
    fn retries_5xx_but_not_4xx() {
        let retry_server = MockServer::start(|request, index| {
            if request.path.starts_with("/v1/embed/image") && index == 0 {
                MockResponse::json(503, r#"{"message":"模型加载中"}"#)
            } else {
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "status": "success",
                        "model": "m",
                        "dim": 2,
                        "vector": [1.0, 0.0]
                    })
                    .to_string(),
                )
            }
        });
        let client = AiClient::new(&retry_server.url());
        assert!(client.embed_image("m", b"x").is_ok());
        assert_eq!(retry_server.requests().len(), 2);

        let reject_server = MockServer::start(|_, _| {
            MockResponse::json(422, r#"{"error":"decode","message":"无法解码图片"}"#)
        });
        let client = AiClient::new(&reject_server.url());
        let error = client.embed_image("m", b"x").unwrap_err();
        assert!(error.message.contains("无法解码图片"));
        assert_eq!(reject_server.requests().len(), 1);
    }

    #[test]
    fn describe_encodes_prompt_and_parses_text() {
        let server = MockServer::start(|request, _| {
            assert!(request.path.contains("maxTokens=160"));
            assert!(request.path.contains("prompt=%E7%BA%A2%E8%89%B2"));
            MockResponse::json(
                200,
                serde_json::json!({
                    "status": "success",
                    "model": "desc-test",
                    "text": " 一名人物  ",
                    "truncated": false
                })
                .to_string(),
            )
        });
        let client = AiClient::new(&server.url());
        let result = client.describe("红色", Some(160), b"image").unwrap();
        assert_eq!(result.model, "desc-test");
        assert_eq!(result.text, "一名人物");
    }

    #[test]
    fn pose_parses_landmarks() {
        let server = MockServer::start(|_, _| {
            let landmarks: Vec<_> = (0..33)
                .map(|index| {
                    serde_json::json!({
                        "x": index as f64 / 33.0,
                        "y": 0.5,
                        "z": -0.1,
                        "visibility": 0.9
                    })
                })
                .collect();
            MockResponse::json(
                200,
                serde_json::json!({
                    "status": "success",
                    "model": "mediapipe-pose",
                    "landmarks": landmarks
                })
                .to_string(),
            )
        });
        let client = AiClient::new(&server.url());
        let pose = client.pose("mediapipe-pose", b"image").unwrap();
        assert_eq!(pose.landmarks.len(), 33);
        assert!((pose.landmarks[32].x - 32.0 / 33.0).abs() < 1e-6);
    }

    #[test]
    fn normalizes_vectors() {
        let vector = normalize(vec![3.0, 4.0]).unwrap();
        assert!((vector[0] - 0.6).abs() < 1e-6);
        assert!((vector[1] - 0.8).abs() < 1e-6);
        assert!(normalize(vec![0.0, 0.0]).is_err());
        assert!(normalize(vec![f32::NAN]).is_err());
    }

    #[test]
    fn encodes_prompt_query() {
        let url = query_url(
            "http://127.0.0.1:18766/v1/describe",
            &[("prompt", "红色 斗篷/侧身"), ("maxTokens", "160")],
        );
        assert!(
            url.contains("prompt=%E7%BA%A2%E8%89%B2%20%E6%96%97%E7%AF%B7%2F%E4%BE%A7%E8%BA%AB")
        );
        assert!(url.ends_with("maxTokens=160"));
    }

    /// 服务卡住时点取消：不能等这次请求自己超时（最长 120 秒），要立刻返回。
    #[test]
    fn cancel_interrupts_a_request_in_flight() {
        let server = MockServer::start(|_, _| {
            std::thread::sleep(Duration::from_millis(800));
            MockResponse::json(
                200,
                serde_json::json!({
                    "status": "success",
                    "model": "m",
                    "dim": 2,
                    "vector": [1.0, 0.0]
                })
                .to_string(),
            )
        });
        let flag = Arc::new(AtomicBool::new(false));
        let cancel_flag = flag.clone();
        let client = AiClient::cancellable(
            &server.url(),
            Arc::new(move || cancel_flag.load(Ordering::SeqCst)),
        );
        let trigger = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            trigger.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let error = client.embed_image("m", b"x").unwrap_err();
        let elapsed = started.elapsed();
        assert!(error.cancelled, "应返回取消而不是失败：{}", error.message);
        assert!(
            elapsed < Duration::from_millis(600),
            "取消应立刻返回，实际等了 {elapsed:?}"
        );
    }

    /// 取消后再调用不再发请求，也不重试。
    #[test]
    fn cancelled_client_skips_the_request() {
        let server = MockServer::start(|_, _| MockResponse::json(200, "{}"));
        let client = AiClient::cancellable(&server.url(), Arc::new(|| true));
        let error = client.embed_image("m", b"x").unwrap_err();
        assert!(error.cancelled);
        assert!(server.requests().is_empty(), "已取消时不应发出请求");
    }

    /// 重试也算在总预算里：服务一直不响应时，最坏等的是 5 分钟，不是「单次超时 × 次数」。
    #[test]
    fn total_budget_stops_retries() {
        let server = MockServer::start(|_, _| {
            std::thread::sleep(Duration::from_millis(400));
            MockResponse::json(503, r#"{"error":"busy"}"#)
        });
        let client = AiClient::new(&server.url())
            .with_limits(Duration::from_millis(120), Duration::from_millis(260));
        let started = Instant::now();
        let error = client.embed_image("m", b"x").unwrap_err();
        assert!(error.cancelled, "预算用完应停止等待：{}", error.message);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "预算应约束总等待，实际 {:?}",
            started.elapsed()
        );
    }
}

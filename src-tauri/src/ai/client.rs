use std::{collections::HashMap, fmt, io::Read, time::Duration};

use serde::{Deserialize, Serialize};

use crate::library::viewer::{PoseData, PoseLandmark};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
const EMBED_TIMEOUT: Duration = Duration::from_secs(60);
const POSE_TIMEOUT: Duration = Duration::from_secs(60);
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(300);
const ENSURE_TIMEOUT: Duration = Duration::from_secs(600);

/// `/v1` 调用失败。`retryable` 决定 `with_retry` 是否再试：网络错误与 5xx 为 true，4xx 与响应格式错误为 false。
#[derive(Debug, Clone)]
pub struct AiError {
    pub message: String,
    pub retryable: bool,
}

impl AiError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    fn status(code: u16, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: (500..=599).contains(&code),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
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

pub struct AiClient {
    base_url: String,
    agent: ureq::Agent,
}

impl AiClient {
    /// `base_url` 去掉首尾空白与结尾斜杠；所有请求共用同一个 agent（连接复用）。
    pub fn new(base_url: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .build();
        Self {
            base_url: base_url.trim().trim_end_matches('/').to_owned(),
            agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    pub fn health(&self) -> Result<Health, AiError> {
        let response = self
            .agent
            .get(&self.url("/v1/health"))
            .timeout(HEALTH_TIMEOUT)
            .call()
            .map_err(map_error)?;
        parse_json(response)
    }

    pub fn ensure_models(
        &self,
        capabilities: &[String],
    ) -> Result<HashMap<String, Capability>, AiError> {
        let payload = serde_json::json!({ "capabilities": capabilities });
        with_retry(|| {
            let response = self
                .agent
                .post(&self.url("/v1/models/ensure"))
                .timeout(ENSURE_TIMEOUT)
                .set("Content-Type", "application/json; charset=utf-8")
                .send_string(&payload.to_string())
                .map_err(map_error)?;
            let parsed: CapabilitiesResponse = parse_json(response)?;
            Ok(parsed.capabilities)
        })
    }

    pub fn embed_image(&self, model: &str, bytes: &[u8]) -> Result<Embedding, AiError> {
        let url = query_url(&self.url("/v1/embed/image"), &[("model", model)]);
        with_retry(|| {
            let response = self
                .agent
                .post(&url)
                .timeout(EMBED_TIMEOUT)
                .set("Content-Type", "application/octet-stream")
                .send_bytes(bytes)
                .map_err(map_error)?;
            let parsed: EmbeddingResponse = parse_json(response)?;
            embedding_from(parsed)
        })
    }

    pub fn embed_text(&self, model: &str, text: &str) -> Result<Embedding, AiError> {
        let payload = serde_json::json!({ "text": text, "model": model });
        with_retry(|| {
            let response = self
                .agent
                .post(&self.url("/v1/embed/text"))
                .timeout(EMBED_TIMEOUT)
                .set("Content-Type", "application/json; charset=utf-8")
                .send_string(&payload.to_string())
                .map_err(map_error)?;
            let parsed: EmbeddingResponse = parse_json(response)?;
            embedding_from(parsed)
        })
    }

    pub fn pose(&self, model: &str, bytes: &[u8]) -> Result<PoseData, AiError> {
        let url = query_url(&self.url("/v1/pose"), &[("model", model)]);
        with_retry(|| {
            let response = self
                .agent
                .post(&url)
                .timeout(POSE_TIMEOUT)
                .set("Content-Type", "application/octet-stream")
                .send_bytes(bytes)
                .map_err(map_error)?;
            let parsed: PoseResponse = parse_json(response)?;
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
        with_retry(|| {
            let response = self
                .agent
                .post(&url)
                .timeout(DESCRIBE_TIMEOUT)
                .set("Content-Type", "application/octet-stream")
                .send_bytes(bytes)
                .map_err(map_error)?;
            let parsed: DescribeResponse = parse_json(response)?;
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
        self.agent
            .post(&self.url("/v1/shutdown"))
            .timeout(Duration::from_secs(2))
            .send_string("")
            .map(|_| ())
            .map_err(map_error)
    }
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

fn parse_json<T: for<'de> Deserialize<'de>>(response: ureq::Response) -> Result<T, AiError> {
    let body = response
        .into_string()
        .map_err(|error| AiError::transport(format!("读取 AI 服务响应失败：{error}")))?;
    serde_json::from_str(&body)
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

/// 网络错误与 5xx 重试 2 次（250ms、1s 退避）；4xx 立即失败。
fn with_retry<T>(mut call: impl FnMut() -> Result<T, AiError>) -> Result<T, AiError> {
    const DELAYS_MS: [u64; 2] = [250, 1000];
    for attempt in 0..=DELAYS_MS.len() {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(DELAYS_MS[attempt - 1]));
        }
        match call() {
            Ok(value) => return Ok(value),
            Err(error) if !error.retryable || attempt == DELAYS_MS.len() => return Err(error),
            Err(_) => {}
        }
    }
    unreachable!("重试循环最后一次尝试一定返回")
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
}

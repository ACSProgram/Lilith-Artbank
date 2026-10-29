//! 测试进程内的假 `/v1` 服务：只覆盖协议、错误路径与确定性向量，不加载模型。
//!
//! 监听 `127.0.0.1:0`，同一时间处理一个连接；响应带 `Connection: close`，
//! ureq 每次都会重新建连，测试无需处理 keep-alive。

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use serde_json::{Value, json};

/// 5xx 重试测试的全局尝试次数；测试开始前重置，避免同一进程里的其它用例污染。
static RETRY_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn reset_retry_attempts() {
    RETRY_ATTEMPTS.store(0, Ordering::SeqCst);
}

#[derive(Debug, Clone)]
pub struct StubRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

impl StubRequest {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Debug, Clone)]
pub struct StubConfig {
    pub image_model: String,
    pub image_dim: u32,
    pub text_model: String,
    pub text_dim: u32,
    pub pose_model: String,
    pub description_model: String,
}

impl Default for StubConfig {
    fn default() -> Self {
        Self {
            image_model: "stub-image".into(),
            image_dim: 8,
            text_model: "stub-image".into(),
            text_dim: 8,
            pose_model: "stub-pose".into(),
            description_model: "stub-description".into(),
        }
    }
}

pub struct Stub {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    requests: Arc<Mutex<Vec<StubRequest>>>,
    config: Arc<Mutex<StubConfig>>,
}

impl Stub {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let config = Arc::new(Mutex::new(StubConfig::default()));
        let requests_for_thread = requests.clone();
        let config_for_thread = config.clone();
        let stop_for_thread = stop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("acceptance-ai-stub".to_owned())
            .spawn(move || {
                ready_tx.send(()).ok();
                while !stop_for_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let requests = requests_for_thread.clone();
                            let config = config_for_thread.clone();
                            std::thread::spawn(move || {
                                stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
                                let _ = serve(&mut stream, &requests, &config);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(3));
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("无法启动假 AI 服务");
        let _ = ready_rx.recv();
        Self {
            address,
            stop,
            handle: Some(handle),
            requests,
            config,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    pub fn requests(&self) -> Vec<StubRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn requests_to(&self, path: &str) -> Vec<StubRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.path.starts_with(path))
            .collect()
    }

    pub fn config(&self) -> StubConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn update_config(&self, update: impl FnOnce(&mut StubConfig)) {
        update(&mut self.config.lock().unwrap());
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 同一条连接可复用：ureq 的重试会沿用连接池，服务器提前关闭会在 Windows 上表现为连接被中止。
fn serve(
    stream: &mut TcpStream,
    requests: &Mutex<Vec<StubRequest>>,
    config: &Mutex<StubConfig>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let header_end = loop {
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
            match stream.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                Err(_) => return Ok(()),
            }
            if buffer.len() > 1 << 20 {
                return Ok(());
            }
        };
        let header = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let mut lines = header.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let content_length = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .next()
            .unwrap_or(0);
        let body_start = header_end + 4;
        while buffer.len() < body_start + content_length {
            match stream.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                Err(_) => return Ok(()),
            }
        }
        let body = buffer[body_start..body_start + content_length].to_vec();
        buffer.drain(..body_start + content_length);
        let request = StubRequest {
            method,
            path: path.split('?').next().unwrap_or_default().to_owned(),
            body,
        };
        let index = {
            let mut requests = requests.lock().unwrap();
            requests.push(request.clone());
            requests.len() - 1
        };
        let (status, payload) = respond(&request, index, &config.lock().unwrap());
        let reason = match status {
            200 => "OK",
            422 => "Unprocessable Entity",
            503 => "Service Unavailable",
            _ => "Error",
        };
        let body = payload.to_string();
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body.as_bytes())?;
        stream.flush()?;
    }
}

fn respond(request: &StubRequest, _index: usize, config: &StubConfig) -> (u16, Value) {
    let text = request.body_text();
    match request.path.as_str() {
        "/v1/health" => (
            200,
            json!({
                "status": "ok",
                "version": "stub",
                "capabilities": {
                    "imageEmbedding": {
                        "ready": true,
                        "model": config.image_model,
                        "dim": config.image_dim,
                    },
                    "textEmbedding": {
                        "ready": true,
                        "model": config.text_model,
                        "dim": config.text_dim,
                    },
                    "pose": { "ready": true, "model": config.pose_model },
                    "description": { "ready": true, "model": config.description_model },
                },
            }),
        ),
        "/v1/models/ensure" => {
            let requested: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let mut capabilities = serde_json::Map::new();
            for name in requested["capabilities"].as_array().into_iter().flatten() {
                let name = name.as_str().unwrap_or_default();
                let entry = match name {
                    "imageEmbedding" => Some(json!({
                        "ready": true,
                        "model": config.image_model,
                        "dim": config.image_dim,
                    })),
                    "textEmbedding" => Some(json!({
                        "ready": true,
                        "model": config.text_model,
                        "dim": config.text_dim,
                    })),
                    "pose" => Some(json!({ "ready": true, "model": config.pose_model })),
                    "description" => {
                        Some(json!({ "ready": true, "model": config.description_model }))
                    }
                    _ => None,
                };
                if let Some(entry) = entry {
                    capabilities.insert(name.to_owned(), entry);
                }
            }
            (200, json!({ "capabilities": capabilities }))
        }
        "/v1/embed/image" => (
            200,
            json!({
                "status": "success",
                "model": config.image_model,
                "dim": config.image_dim,
                "vector": image_vector(&request.body, config.image_dim),
            }),
        ),
        "/v1/embed/text" => embed_text(&text, config),
        "/v1/pose" => (
            200,
            json!({
                "status": "success",
                "model": config.pose_model,
                "landmarks": (0..33)
                    .map(|index| json!({
                        "x": f64::from(index) / 33.0,
                        "y": f64::from(index) / 66.0,
                        "z": 0.0,
                        "visibility": 0.9,
                    }))
                    .collect::<Vec<_>>(),
            }),
        ),
        "/v1/describe" => (
            200,
            json!({
                "status": "success",
                "model": config.description_model,
                "text": "stub 描述",
            }),
        ),
        "/v1/shutdown" => (200, json!({ "status": "ok" })),
        _ => (404, json!({ "error": "未知路径" })),
    }
}

fn embed_text(body: &str, config: &StubConfig) -> (u16, Value) {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let text = parsed["text"].as_str().unwrap_or_default();
    if text.contains("reject") {
        return (422, json!({ "error": "请求被拒绝" }));
    }
    let mut vector = vec![0.0_f32; config.text_dim as usize];
    if vector.is_empty() {
        vector.push(1.0);
    }
    if text.contains("retry") {
        // 重试测试：前两次调用返回 503，第三次才成功。
        let attempt = RETRY_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            return (503, json!({ "error": "暂时不可用" }));
        }
    }
    if text.contains("bad-dim") {
        vector.truncate(vector.len().saturating_sub(1));
        return (
            200,
            json!({
                "status": "success",
                "model": config.text_model,
                "dim": config.text_dim,
                "vector": vector,
            }),
        );
    }
    vector[0] = 1.0;
    (
        200,
        json!({
            "status": "success",
            "model": config.text_model,
            "dim": config.text_dim,
            "vector": vector,
        }),
    )
}

/// 4 个象限的平均 RGB（8 维）：轻微像素差异只会让向量微动，适合验证语义分组。
fn image_vector(bytes: &[u8], dim: u32) -> Vec<f32> {
    let mut vector = vec![0.0_f32; dim.max(1) as usize];
    let Ok(image) = image::load_from_memory(bytes) else {
        vector[0] = 1.0;
        return vector;
    };
    let rgb = image.to_rgb8();
    let (width, height) = rgb.dimensions();
    if width == 0 || height == 0 {
        vector[0] = 1.0;
        return vector;
    }
    let mut sums = [[0.0_f32; 3]; 4];
    let mut counts = [0.0_f32; 4];
    for (x, y, pixel) in rgb.enumerate_pixels() {
        let quadrant = usize::from(y >= height / 2) * 2 + usize::from(x >= width / 2);
        for channel in 0..3 {
            sums[quadrant][channel] += f32::from(pixel[channel]);
        }
        counts[quadrant] += 1.0;
    }
    for (quadrant, sums) in sums.iter().enumerate() {
        for (channel, sum) in sums.iter().enumerate().take(2) {
            let value = if counts[quadrant] > 0.0 {
                *sum / counts[quadrant] / 255.0
            } else {
                0.0
            };
            let index = quadrant * 2 + channel;
            if index < vector.len() {
                vector[index] = value;
            }
        }
    }
    vector
}

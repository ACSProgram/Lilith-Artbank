use tauri::{
    AppHandle, Manager, UriSchemeResponder, Wry,
    http::{Response, StatusCode, header},
};

use crate::{
    app::state::AppState,
    library::thumbs::{self, ThumbPointer},
};

const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

enum ThumbError {
    BadRequest(&'static str),
    NotFound(&'static str),
    Internal(String),
}

/// `thumb://` 协议：Windows 上表现为 `http://thumb.localhost/<id>`。
pub fn register(builder: tauri::Builder<Wry>) -> tauri::Builder<Wry> {
    builder.register_asynchronous_uri_scheme_protocol("thumb", |context, request, responder| {
        let app = context.app_handle().clone();
        let path = request.uri().path().trim_start_matches('/').to_owned();
        tauri::async_runtime::spawn_blocking(move || {
            respond(&app, &path, responder);
        });
    })
}

fn respond(app: &AppHandle<Wry>, path: &str, responder: UriSchemeResponder) {
    let response = match route(app, path) {
        Ok(response) => response,
        Err(ThumbError::BadRequest(reason)) => {
            log::warn!("缩略图请求 {path} 无效：{reason}");
            empty(StatusCode::BAD_REQUEST)
        }
        Err(ThumbError::NotFound(reason)) => {
            log::debug!("缩略图 {path} 不可用：{reason}");
            empty(StatusCode::NOT_FOUND)
        }
        Err(ThumbError::Internal(reason)) => {
            log::warn!("缩略图 {path} 读取失败：{reason}");
            empty(StatusCode::INTERNAL_SERVER_ERROR)
        }
    };
    responder.respond(response);
}

fn route(app: &AppHandle<Wry>, path: &str) -> Result<Response<Vec<u8>>, ThumbError> {
    let image_id = path
        .parse::<i64>()
        .map_err(|_| ThumbError::BadRequest("图片 id 无效"))?;
    let state = app.state::<AppState>();
    let (database, pack) = state
        .current_thumb_source()
        .ok_or(ThumbError::NotFound("图库未打开"))?;
    let pointer: ThumbPointer = thumbs::load_pointer(&database, image_id)
        .map_err(|error| ThumbError::Internal(error.to_string()))?
        .ok_or(ThumbError::NotFound("图片没有缩略图"))?;
    let bytes = pack.read(pointer.location).map_err(|error| {
        log::warn!("图片 {image_id} 的缩略图不可读：{error}");
        ThumbError::NotFound("缩略图包缺失或指针失效")
    })?;
    Ok(image_response(bytes, &pointer.format, pointer.version))
}

fn image_response(bytes: Vec<u8>, format: &str, version: u32) -> Response<Vec<u8>> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(format))
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .header("X-Content-Type-Options", "nosniff");
    if version > 0 {
        builder = builder.header(header::ETAG, format!("\"v{version}\""));
    }
    builder
        .body(bytes)
        .expect("thumbnail response is always valid")
}

fn empty(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_LENGTH, "0")
        .body(Vec::new())
        .expect("empty response is always valid")
}

fn content_type(format: &str) -> &'static str {
    match format {
        "webp" => "image/webp",
        "jpeg" | "jpg" => "image/jpeg",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

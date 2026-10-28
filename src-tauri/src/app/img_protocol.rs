use tauri::{
    AppHandle, Manager, UriSchemeResponder, Wry,
    http::{Response, StatusCode, header},
};

use crate::{
    app::state::AppState,
    library::{
        content::{self, ContentRow},
        db::LibraryDb,
        paths::LibraryPaths,
        viewer,
    },
};

const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

enum ImageError {
    BadRequest(&'static str),
    NotFound(String),
}

/// `img://` 协议：原文表现为 `http://img.localhost/<id>?v=<hash>`，
/// 降采样预览为 `http://img.localhost/preview/<id>?v=<hash>`。
pub fn register(builder: tauri::Builder<Wry>) -> tauri::Builder<Wry> {
    builder.register_asynchronous_uri_scheme_protocol("img", |context, request, responder| {
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
        Err(ImageError::BadRequest(reason)) => {
            log::warn!("原图请求 {path} 无效：{reason}");
            empty(StatusCode::BAD_REQUEST)
        }
        Err(ImageError::NotFound(reason)) => {
            log::debug!("原图 {path} 不可用：{reason}");
            empty(StatusCode::NOT_FOUND)
        }
    };
    responder.respond(response);
}

fn route(app: &AppHandle<Wry>, path: &str) -> Result<Response<Vec<u8>>, ImageError> {
    let state = app.state::<AppState>();
    let (database, root) = state
        .current_library()
        .ok_or_else(|| ImageError::NotFound("图库未打开".into()))?;
    let paths = LibraryPaths::new(&root);

    if let Some(raw_id) = path.strip_prefix("preview/") {
        let image_id = raw_id
            .parse::<i64>()
            .map_err(|_| ImageError::BadRequest("图片 id 无效"))?;
        let row = load_row(&database, image_id)?;
        let (target, content_hash) = viewer::ensure_preview_file(
            &database,
            &paths,
            &row,
            &state.resources(),
            &state.images(),
        )
        .map_err(|error| ImageError::NotFound(format!("预览文件不可用：{error}")))?;
        let bytes = std::fs::read(&target)
            .map_err(|error| ImageError::NotFound(format!("预览文件不可用：{error}")))?;
        return Ok(image_response(
            bytes,
            "image/webp",
            &format!("{content_hash}-preview"),
        ));
    }

    let image_id = path
        .parse::<i64>()
        .map_err(|_| ImageError::BadRequest("图片 id 无效"))?;
    let holder = crate::concurrency::resources::HolderInfo::new(
        format!("img-protocol-{image_id}"),
        "查看器",
    )
    .with_path_note("正在读取");
    let (row, bytes) = crate::library::content::try_read_original(
        &database,
        &state.resources(),
        &root,
        image_id,
        &holder,
    )
    .map_err(|error| ImageError::NotFound(format!("原图文件不可用：{error}")))?;
    Ok(image_response(
        bytes,
        content_type(&row.file_ext),
        &row.content_hash,
    ))
}

/// 行缺失与读取失败对前端是同一件事：这张图当前不可用，按 404 让 `<img>` 走失败态。
fn load_row(database: &LibraryDb, image_id: i64) -> Result<ContentRow, ImageError> {
    content::load(database, image_id)
        .map_err(|error| ImageError::NotFound(format!("图片不可用：{error}")))
}

fn image_response(bytes: Vec<u8>, content_type: &'static str, etag: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .header(header::ETAG, format!("\"{etag}\""))
        .header("X-Content-Type-Options", "nosniff")
        .body(bytes)
        .expect("image response is always valid")
}

fn empty(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_LENGTH, "0")
        .body(Vec::new())
        .expect("empty response is always valid")
}

fn content_type(extension: &str) -> &'static str {
    match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
}

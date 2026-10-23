use std::path::Path;

use crate::{
    app::error::{AppError, AppResult},
    imageops::{
        edit::{self, OutputFormat},
        thumbnail,
    },
    import::decode,
    library::{content, db::LibraryDb, paths::LibraryPaths, thumbs, thumbs::ThumbPack},
};

const DESCRIPTION_MAX_EDGE: u32 = 1536;
const SMALL_MAX_EDGE: u32 = 512;
const DESCRIPTION_QUALITY: u8 = 85;

/// embedding / pose 优先使用 512px 缩略图，避免为批量任务重复解码原图。
pub fn small_bytes(
    database: &LibraryDb,
    thumbs: &ThumbPack,
    root: &Path,
    image_id: i64,
) -> AppResult<Vec<u8>> {
    if let Some(pointer) = thumbs::load_pointer(database, image_id)?
        && let Ok(bytes) = thumbs.read(pointer.location)
        && !bytes.is_empty()
    {
        return Ok(bytes);
    }
    encoded_from_original(database, root, image_id, SMALL_MAX_EDGE)
}

/// 描述模型使用最长边 1536 的 WebP，避免把超大原图直接上传。
pub fn description_bytes(database: &LibraryDb, root: &Path, image_id: i64) -> AppResult<Vec<u8>> {
    encoded_from_original(database, root, image_id, DESCRIPTION_MAX_EDGE)
}

fn encoded_from_original(
    database: &LibraryDb,
    root: &Path,
    image_id: i64,
    max_edge: u32,
) -> AppResult<Vec<u8>> {
    let row = content::load(database, image_id)?;
    let paths = LibraryPaths::new(root);
    let path = row.path(&paths)?;
    if !path.is_file() {
        return Err(AppError::message("原图文件不存在"));
    }
    let decoded = decode::decode(&path)?;
    let (width, height) = thumbnail::fit_within(decoded.width, decoded.height, max_edge);
    let image = if (width, height) == (decoded.width, decoded.height) {
        decoded.image
    } else {
        decoded
            .image
            .resize_exact(width, height, image::imageops::FilterType::Lanczos3)
    };
    let mut bytes = Vec::new();
    edit::encode(&image, OutputFormat::WebP, DESCRIPTION_QUALITY, &mut bytes)?;
    Ok(bytes)
}

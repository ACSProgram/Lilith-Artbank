use std::path::Path;

use crate::{
    app::error::{AppError, AppResult},
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget, estimate_working_bytes},
        resources::{HolderInfo, ResourceTable},
    },
    imageops::{
        edit::{self, OutputFormat},
        thumbnail,
    },
    import::decode,
    library::{content, db::LibraryDb, thumbs, thumbs::ThumbPack},
};

const DESCRIPTION_MAX_EDGE: u32 = 1536;
const SMALL_MAX_EDGE: u32 = 512;
const DESCRIPTION_QUALITY: u8 = 85;

/// embedding / pose 优先使用 512px 缩略图，避免为批量任务重复解码原图。
#[allow(clippy::too_many_arguments)]
pub fn small_bytes(
    database: &LibraryDb,
    thumbs: &ThumbPack,
    root: &Path,
    image_id: i64,
    resources: &ResourceTable,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
    holder: &HolderInfo,
    cancel: &dyn Fn() -> bool,
) -> AppResult<Vec<u8>> {
    if let Some(pointer) = thumbs::load_pointer(database, image_id)?
        && let Ok(bytes) = thumbs.read(pointer.location)
        && !bytes.is_empty()
    {
        return Ok(bytes);
    }
    encoded_from_original(
        database,
        root,
        image_id,
        SMALL_MAX_EDGE,
        resources,
        images,
        memory,
        holder,
        cancel,
    )
}

/// 描述模型使用最长边 1536 的 WebP，避免把超大原图直接上传。
#[allow(clippy::too_many_arguments)]
pub fn description_bytes(
    database: &LibraryDb,
    root: &Path,
    image_id: i64,
    resources: &ResourceTable,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
    holder: &HolderInfo,
    cancel: &dyn Fn() -> bool,
) -> AppResult<Vec<u8>> {
    encoded_from_original(
        database,
        root,
        image_id,
        DESCRIPTION_MAX_EDGE,
        resources,
        images,
        memory,
        holder,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn encoded_from_original(
    database: &LibraryDb,
    root: &Path,
    image_id: i64,
    max_edge: u32,
    resources: &ResourceTable,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
    holder: &HolderInfo,
    cancel: &dyn Fn() -> bool,
) -> AppResult<Vec<u8>> {
    // 先拿路径读锁并读字节，再拿图像预算：预算许可只在解码/编码期间持有，
    // 不会出现“拿着许可等文件夹写者”的环。线程许可在前、字节额度在后，顺序与其它路径一致。
    let (row, bytes) = content::read_original(database, resources, root, image_id, holder, cancel)?;
    let _permit = images
        .acquire(cancel)
        .map_err(|_| AppError::message("图像预算等待已取消"))?;
    let estimate = estimate_working_bytes(row.width, row.height, bytes.len() as u64, false);
    let _memory = memory
        .acquire(estimate, cancel)
        .map_err(|_| AppError::message("图像内存预算等待已取消"))?;
    let decoded = decode::decode_bytes(&bytes, Some(&row.file_ext))?;
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

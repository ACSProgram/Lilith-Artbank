use std::path::Path;

use crate::{
    app::error::{AppError, AppResult},
    imageops::edit::MAX_EDIT_PIXELS,
};

/// 复制图片需要整份解码为 RGBA，沿用编辑上限，避免一次分配几百 MB。
const MAX_CLIPBOARD_PIXELS: u64 = MAX_EDIT_PIXELS;

fn open() -> AppResult<arboard::Clipboard> {
    arboard::Clipboard::new().map_err(|error| AppError::message(format!("无法打开剪贴板：{error}")))
}

/// 写入系统剪贴板文本；前端不直接读图库目录，路径拼装留在 Rust。
pub fn copy_text(text: &str) -> AppResult<()> {
    if text.is_empty() {
        return Err(AppError::message("没有可复制的内容"));
    }
    open()?
        .set_text(text.to_owned())
        .map_err(|error| AppError::message(format!("无法写入剪贴板：{error}")))
}

/// 复制解码后的图片像素（应用 EXIF 方向），而不是原始文件字节。
pub fn copy_image(path: &Path) -> AppResult<()> {
    let decoded = crate::import::decode::decode(path)?;
    let pixels = u64::from(decoded.width) * u64::from(decoded.height);
    if pixels > MAX_CLIPBOARD_PIXELS {
        return Err(AppError::message(format!(
            "图片为 {}×{}，超过复制到剪贴板的上限 {} 百万像素",
            decoded.width,
            decoded.height,
            MAX_CLIPBOARD_PIXELS / 1_000_000
        )));
    }

    let rgba = decoded.image.to_rgba8();
    let (width, height) = rgba.dimensions();
    open()?
        .set_image(arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: std::borrow::Cow::Owned(rgba.into_raw()),
        })
        .map_err(|error| AppError::message(format!("无法复制图片：{error}")))
}

use image::{DynamicImage, imageops::FilterType};

use crate::app::error::{AppError, AppResult};

pub const THUMB_MAX_EDGE: u32 = 512;
pub const THUMB_QUALITY: f32 = 80.0;

#[derive(Debug)]
pub struct EncodedThumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// 最长边缩到 512px，保持宽高比；透明图保留 alpha。
pub fn encode(image: &DynamicImage) -> AppResult<EncodedThumbnail> {
    if image.width() == 0 || image.height() == 0 {
        return Err(AppError::message("图片尺寸无效"));
    }

    let (width, height) = fit_within(image.width(), image.height(), THUMB_MAX_EDGE);
    let resized = if (width, height) == (image.width(), image.height()) {
        None
    } else {
        Some(image.resize_exact(width, height, FilterType::Lanczos3))
    };
    let source = resized.as_ref().unwrap_or(image);

    let encoded = match source {
        DynamicImage::ImageRgb8(buffer) => {
            webp::Encoder::from_rgb(buffer.as_raw(), width, height).encode(THUMB_QUALITY)
        }
        DynamicImage::ImageRgba8(buffer) => {
            webp::Encoder::from_rgba(buffer.as_raw(), width, height).encode(THUMB_QUALITY)
        }
        other => {
            let buffer = other.to_rgba8();
            webp::Encoder::from_rgba(buffer.as_raw(), width, height).encode(THUMB_QUALITY)
        }
    };

    if encoded.is_empty() {
        return Err(AppError::message("WebP 编码结果为空"));
    }

    Ok(EncodedThumbnail {
        bytes: encoded.to_vec(),
        width,
        height,
    })
}

pub fn fit_within(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max_edge {
        return (width, height);
    }
    let scale = max_edge as f64 / longest as f64;
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, Rgb, RgbImage};

    use super::*;

    fn solid(width: u32, height: u32) -> DynamicImage {
        let mut image = RgbImage::new(width, height);
        for pixel in image.pixels_mut() {
            *pixel = Rgb([48, 96, 160]);
        }
        DynamicImage::ImageRgb8(image)
    }

    #[test]
    fn produces_decodable_webp() {
        let encoded = encode(&solid(300, 200)).unwrap();
        assert_eq!((encoded.width, encoded.height), (300, 200));
        let decoded = image::load_from_memory(&encoded.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (300, 200));
    }

    #[test]
    fn shrinks_longest_edge_to_512() {
        let encoded = encode(&solid(1000, 500)).unwrap();
        assert_eq!((encoded.width, encoded.height), (512, 256));
        let decoded = image::load_from_memory(&encoded.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (512, 256));
    }

    #[test]
    fn keeps_small_images_untouched() {
        assert_eq!(fit_within(120, 80, 512), (120, 80));
        assert_eq!(fit_within(1, 10000, 512), (1, 512));
    }
}

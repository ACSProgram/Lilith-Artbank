use std::io::Write;

use image::{
    DynamicImage, ExtendedColorType, ImageBuffer, ImageEncoder, Pixel, RgbImage,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops::{self, FilterType},
};
use serde::Deserialize;

use crate::app::error::{AppError, AppResult};

/// 编辑保存的单图像素上限；超过时给出可行动错误，不做无上限分配。
pub const MAX_EDIT_PIXELS: u64 = 50_000_000;
pub const MAX_EDIT_DIMENSION: u32 = 65_535;
pub const DEFAULT_QUALITY: u8 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Jpeg,
    Png,
    WebP,
}

impl OutputFormat {
    /// 未指定格式时：沿用 JPEG/PNG/WebP；其它格式按是否有 alpha 转 PNG/JPEG。
    pub fn default_for(original: &str, has_alpha: bool) -> Self {
        match original {
            "jpeg" | "jpg" => Self::Jpeg,
            "png" => Self::Png,
            "webp" => Self::WebP,
            _ if has_alpha => Self::Png,
            _ => Self::Jpeg,
        }
    }

    pub fn parse(value: Option<&str>, original: &str, has_alpha: bool) -> AppResult<Self> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None => Ok(Self::default_for(original, has_alpha)),
            Some("jpeg" | "jpg") => Ok(Self::Jpeg),
            Some("png") => Ok(Self::Png),
            Some("webp") => Ok(Self::WebP),
            Some(other) => Err(AppError::message(format!(
                "不支持的输出格式：{other}（可选 jpeg / png / webp）"
            ))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::WebP => "webp",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::WebP => "webp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeSpec {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Geometry {
    /// 顺时针 90° 的次数（0-3）。
    pub rotate: u8,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
    /// 在翻转+旋转后的坐标系里裁剪。
    pub crop: Option<CropRect>,
    /// 在裁剪后的坐标系里缩放。
    pub resize: Option<ResizeSpec>,
}

impl Geometry {
    pub fn validate(&self) -> AppResult<()> {
        if self.rotate > 3 {
            return Err(AppError::message("旋转参数无效"));
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.rotate == 0
            && !self.flip_horizontal
            && !self.flip_vertical
            && self.crop.is_none()
            && self.resize.is_none()
    }
}

/// 翻转 → 旋转 → 裁剪 → 缩放；与前端 CSS `rotate(deg) scaleX() scaleY()` 的顺序一致。
pub fn apply(image: DynamicImage, geometry: &Geometry) -> AppResult<DynamicImage> {
    geometry.validate()?;
    let pixel_count = u64::from(image.width()) * u64::from(image.height());
    if pixel_count > MAX_EDIT_PIXELS {
        return Err(AppError::message(format!(
            "图片为 {}×{}（{:.1} 百万像素），超过编辑上限 {} 百万像素",
            image.width(),
            image.height(),
            pixel_count as f64 / 1_000_000.0,
            MAX_EDIT_PIXELS / 1_000_000
        )));
    }

    if image.color().has_alpha() {
        let buffer = image.into_rgba8();
        Ok(DynamicImage::ImageRgba8(transform(buffer, geometry)?))
    } else {
        let buffer = image.into_rgb8();
        Ok(DynamicImage::ImageRgb8(transform(buffer, geometry)?))
    }
}

fn transform<P>(
    mut buffer: ImageBuffer<P, Vec<u8>>,
    geometry: &Geometry,
) -> AppResult<ImageBuffer<P, Vec<u8>>>
where
    P: Pixel<Subpixel = u8> + 'static,
{
    if geometry.flip_horizontal {
        imageops::flip_horizontal_in_place(&mut buffer);
    }
    if geometry.flip_vertical {
        imageops::flip_vertical_in_place(&mut buffer);
    }

    if geometry.rotate != 0 {
        let rotated = match geometry.rotate {
            1 => imageops::rotate90(&buffer),
            2 => imageops::rotate180(&buffer),
            3 => imageops::rotate270(&buffer),
            _ => unreachable!("rotate validated above"),
        };
        buffer = rotated;
    }

    if let Some(crop) = geometry.crop {
        validate_crop(crop, buffer.width(), buffer.height())?;
        buffer = imageops::crop_imm(&buffer, crop.x, crop.y, crop.width, crop.height).to_image();
    }

    if let Some(resize) = geometry.resize {
        validate_resize(resize)?;
        if (resize.width, resize.height) != (buffer.width(), buffer.height()) {
            buffer = imageops::resize(&buffer, resize.width, resize.height, FilterType::Lanczos3);
        }
    }

    Ok(buffer)
}

fn validate_crop(crop: CropRect, width: u32, height: u32) -> AppResult<()> {
    if crop.width == 0 || crop.height == 0 {
        return Err(AppError::message("裁剪区域不能为空"));
    }
    let right = crop
        .x
        .checked_add(crop.width)
        .ok_or_else(|| AppError::message("裁剪区域超出图片范围"))?;
    let bottom = crop
        .y
        .checked_add(crop.height)
        .ok_or_else(|| AppError::message("裁剪区域超出图片范围"))?;
    if right > width || bottom > height {
        return Err(AppError::message(format!(
            "裁剪区域 {}×{} @ {},{} 超出图片范围 {}×{}",
            crop.width, crop.height, crop.x, crop.y, width, height
        )));
    }
    Ok(())
}

fn validate_resize(resize: ResizeSpec) -> AppResult<()> {
    if resize.width == 0 || resize.height == 0 {
        return Err(AppError::message("目标尺寸必须大于 0"));
    }
    if resize.width > MAX_EDIT_DIMENSION || resize.height > MAX_EDIT_DIMENSION {
        return Err(AppError::message(format!(
            "目标尺寸超过单边上限 {MAX_EDIT_DIMENSION}"
        )));
    }
    let pixels = u64::from(resize.width) * u64::from(resize.height);
    if pixels > MAX_EDIT_PIXELS {
        return Err(AppError::message(format!(
            "目标尺寸 {}×{} 超过编辑上限 {} 百万像素",
            resize.width,
            resize.height,
            MAX_EDIT_PIXELS / 1_000_000
        )));
    }
    Ok(())
}

/// 是否真的需要写新内容；有损格式调整到非默认质量也算一次修改。
pub fn has_changes(
    geometry: &Geometry,
    original_format: &str,
    output: OutputFormat,
    quality: u8,
) -> bool {
    !geometry.is_empty()
        || source_format(original_format) != Some(output)
        || (output != OutputFormat::Png && quality != DEFAULT_QUALITY)
}

/// 原图本身可直接对应的输出格式；GIF/TIFF/TGA/BMP 等需要转换。
fn source_format(original: &str) -> Option<OutputFormat> {
    match original {
        "jpeg" | "jpg" => Some(OutputFormat::Jpeg),
        "png" => Some(OutputFormat::Png),
        "webp" => Some(OutputFormat::WebP),
        _ => None,
    }
}

pub fn encode<W: Write>(
    image: &DynamicImage,
    format: OutputFormat,
    quality: u8,
    writer: W,
) -> AppResult<()> {
    if !(1..=100).contains(&quality) {
        return Err(AppError::message("输出质量必须在 1-100 之间"));
    }

    match format {
        OutputFormat::Jpeg => {
            let rgb = flatten_on_white(image);
            JpegEncoder::new_with_quality(writer, quality).write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                ExtendedColorType::Rgb8,
            )?;
        }
        OutputFormat::Png => {
            let encoder = PngEncoder::new(writer);
            if image.color().has_alpha() {
                let rgba = image.to_rgba8();
                encoder.write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    ExtendedColorType::Rgba8,
                )?;
            } else {
                let rgb = image.to_rgb8();
                encoder.write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    ExtendedColorType::Rgb8,
                )?;
            }
        }
        OutputFormat::WebP => {
            let encoded = if image.color().has_alpha() {
                let rgba = image.to_rgba8();
                webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
                    .encode(f32::from(quality))
            } else {
                let rgb = image.to_rgb8();
                webp::Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height())
                    .encode(f32::from(quality))
            };
            if encoded.is_empty() {
                return Err(AppError::message("WebP 编码结果为空"));
            }
            let mut writer = writer;
            writer.write_all(&encoded)?;
        }
    }
    Ok(())
}

/// JPEG 不支持透明；透明像素按白底合成，避免留下无意义的 RGB 值。
fn flatten_on_white(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut output = Vec::with_capacity(rgba.as_raw().len() / 4 * 3);
    for pixel in rgba.as_raw().chunks_exact(4) {
        let alpha = u16::from(pixel[3]);
        let inverse = 255 - alpha;
        for channel in &pixel[..3] {
            let blended = (u16::from(*channel) * alpha + 255 * inverse + 127) / 255;
            output.push(blended as u8);
        }
    }
    RgbImage::from_raw(width, height, output).expect("flattened buffer has the expected size")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{Rgb, Rgba, RgbaImage};

    use super::*;

    fn marked() -> DynamicImage {
        let mut image = RgbImage::new(2, 2);
        image.put_pixel(0, 0, Rgb([255, 0, 0]));
        image.put_pixel(1, 0, Rgb([0, 255, 0]));
        image.put_pixel(0, 1, Rgb([0, 0, 255]));
        image.put_pixel(1, 1, Rgb([255, 255, 0]));
        DynamicImage::ImageRgb8(image)
    }

    fn rgb(image: &DynamicImage, x: u32, y: u32) -> [u8; 3] {
        let pixel = image.to_rgb8().get_pixel(x, y).0;
        [pixel[0], pixel[1], pixel[2]]
    }

    #[test]
    fn applies_flip_before_rotation() {
        let geometry = Geometry {
            rotate: 1,
            flip_horizontal: true,
            ..Geometry::default()
        };
        let result = apply(marked(), &geometry).unwrap();
        assert_eq!((result.width(), result.height()), (2, 2));
        // 原图: A B / C D → 水平翻转: B A / D C → 顺时针 90°: D B / C A
        assert_eq!(rgb(&result, 0, 0), [255, 255, 0]);
        assert_eq!(rgb(&result, 1, 0), [0, 255, 0]);
        assert_eq!(rgb(&result, 0, 1), [0, 0, 255]);
        assert_eq!(rgb(&result, 1, 1), [255, 0, 0]);
    }

    #[test]
    fn crops_in_transformed_coordinates() {
        let mut image = RgbImage::new(6, 4);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([x as u8 * 10, y as u8 * 10, 7]);
        }
        let geometry = Geometry {
            crop: Some(CropRect {
                x: 1,
                y: 1,
                width: 3,
                height: 2,
            }),
            ..Geometry::default()
        };
        let result = apply(DynamicImage::ImageRgb8(image), &geometry).unwrap();
        assert_eq!((result.width(), result.height()), (3, 2));
        assert_eq!(rgb(&result, 0, 0), [10, 10, 7]);
        assert_eq!(rgb(&result, 2, 1), [30, 20, 7]);
    }

    /// 前端“裁剪 → 旋转 → 水平翻转”会把裁剪矩形一起变换；这里验证等价于
    /// “先旋转整图、再做显示空间水平翻转、最后裁同一块”。
    #[test]
    fn composed_rotate_then_flip_matches_display_sequence() {
        let mut image = RgbImage::new(6, 4);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([x as u8 * 20, y as u8 * 30, 11]);
        }
        let source = DynamicImage::ImageRgb8(image);

        // 初始裁剪 (1,1,3,2) → 顺时针 90° 后 (1,1,2,3) → 显示空间水平翻转后 (1,1,2,3)。
        let geometry = Geometry {
            rotate: 1,
            flip_vertical: true,
            crop: Some(CropRect {
                x: 1,
                y: 1,
                width: 2,
                height: 3,
            }),
            ..Geometry::default()
        };

        let actual = apply(source.clone(), &geometry).unwrap().to_rgb8();
        let rotated = image::imageops::rotate90(&source.to_rgb8());
        let flipped = image::imageops::flip_horizontal(&rotated);
        let expected = image::imageops::crop_imm(&flipped, 1, 1, 2, 3).to_image();
        assert_eq!(actual.dimensions(), expected.dimensions());
        assert_eq!(actual.as_raw(), expected.as_raw());
    }

    #[test]
    fn resizes_after_cropping() {
        let mut image = RgbImage::new(4, 2);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([(x * 40) as u8, (y * 40) as u8, 9]);
        }
        let geometry = Geometry {
            crop: Some(CropRect {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            }),
            resize: Some(ResizeSpec {
                width: 8,
                height: 4,
            }),
            ..Geometry::default()
        };
        let result = apply(DynamicImage::ImageRgb8(image), &geometry).unwrap();
        assert_eq!((result.width(), result.height()), (8, 4));
    }

    #[test]
    fn rejects_invalid_geometry() {
        assert!(
            apply(
                marked(),
                &Geometry {
                    rotate: 4,
                    ..Geometry::default()
                }
            )
            .is_err()
        );
        assert!(
            apply(
                marked(),
                &Geometry {
                    crop: Some(CropRect {
                        x: 1,
                        y: 1,
                        width: 2,
                        height: 2
                    }),
                    ..Geometry::default()
                }
            )
            .is_err()
        );
        assert!(
            apply(
                marked(),
                &Geometry {
                    resize: Some(ResizeSpec {
                        width: 0,
                        height: 10
                    }),
                    ..Geometry::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn encodes_decodable_formats() {
        let source = marked();
        for format in [OutputFormat::Jpeg, OutputFormat::Png, OutputFormat::WebP] {
            let mut bytes = Vec::new();
            encode(&source, format, 90, &mut bytes).unwrap();
            assert!(!bytes.is_empty());
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
        }
    }

    #[test]
    fn png_preserves_alpha_and_jpeg_flattens_to_white() {
        let mut rgba = RgbaImage::new(1, 1);
        rgba.put_pixel(0, 0, Rgba([255, 0, 0, 0]));
        let source = DynamicImage::ImageRgba8(rgba);

        let mut png = Vec::new();
        encode(&source, OutputFormat::Png, 90, &mut png).unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0[3], 0);

        let mut jpeg = Vec::new();
        encode(&source, OutputFormat::Jpeg, 95, &mut jpeg).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        let pixel = decoded.get_pixel(0, 0).0;
        assert!(pixel.iter().all(|value| *value > 240), "{pixel:?}");
    }

    #[test]
    fn detects_changes() {
        let geometry = Geometry::default();
        assert!(!has_changes(&geometry, "jpeg", OutputFormat::Jpeg, 90));
        assert!(has_changes(&geometry, "png", OutputFormat::Jpeg, 90));
        // GIF 没有可直接沿用的输出格式，任何输出都算一次转换。
        assert!(has_changes(&geometry, "gif", OutputFormat::Jpeg, 90));
        assert!(has_changes(&geometry, "gif", OutputFormat::WebP, 90));
        assert!(has_changes(
            &Geometry {
                rotate: 1,
                ..Geometry::default()
            },
            "jpeg",
            OutputFormat::Jpeg,
            90
        ));
        // 有损格式只改质量也要能保存；PNG 的质量参数没有意义。
        assert!(has_changes(&geometry, "jpeg", OutputFormat::Jpeg, 60));
        assert!(has_changes(&geometry, "webp", OutputFormat::WebP, 60));
        assert!(!has_changes(&geometry, "png", OutputFormat::Png, 60));
    }

    #[test]
    fn parses_output_formats() {
        assert_eq!(
            OutputFormat::parse(None, "png", false).unwrap(),
            OutputFormat::Png
        );
        assert_eq!(
            OutputFormat::parse(None, "tiff", true).unwrap(),
            OutputFormat::Png
        );
        assert_eq!(
            OutputFormat::parse(Some("jpg"), "png", true).unwrap(),
            OutputFormat::Jpeg
        );
        assert!(OutputFormat::parse(Some("gif"), "png", false).is_err());
    }

    #[test]
    fn webp_encoder_writes_through_cursor() {
        let mut cursor = Cursor::new(Vec::new());
        encode(&marked(), OutputFormat::WebP, 80, &mut cursor).unwrap();
        assert!(cursor.into_inner().len() > 10);
    }
}

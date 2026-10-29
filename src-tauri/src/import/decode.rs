use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, metadata::Orientation};

use crate::app::error::{AppError, AppResult};

/// 解码上限：单边 65535，总像素 1 亿（RGBA8 约 400 MB），分配上限 512 MB。
pub const MAX_DECODE_PIXELS: u64 = 100_000_000;
pub const MAX_DECODE_EDGE: u32 = 65_535;
pub const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

pub struct DecodedImage {
    pub image: DynamicImage,
    pub format: &'static str,
    pub has_alpha: bool,
    pub width: u32,
    pub height: u32,
}

/// 只读文件头拿尺寸，不解码像素；导入在申请内存额度前用它估算峰值。
///
/// 格式判断与 [`decode`] 保持一致：魔数优先，认不出来时用扩展名兜底。
pub fn probe_dimensions(path: &Path) -> AppResult<(u32, u32)> {
    let mut reader = ImageReader::open(path)
        .map_err(|error| AppError::message(format!("无法打开图片：{error}")))?
        .with_guessed_format()
        .map_err(|error| AppError::message(format!("无法识别图片格式：{error}")))?;
    if reader.format().is_none()
        && let Some(format) = path
            .extension()
            .and_then(|value| value.to_str())
            .and_then(ImageFormat::from_extension)
    {
        reader.set_format(format);
    }
    let decoder = reader
        .into_decoder()
        .map_err(|error| AppError::message(format!("图片解码器初始化失败：{error}")))?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height)?;
    Ok((width, height))
}

/// 解码并应用 EXIF 方向；方向只影响派生数据与显示，不改原图字节。
///
/// 格式优先按魔数判断；魔数不认识时（TGA 没有稳定魔数）用文件扩展名兜底，
/// 这也是临时副本要保留源扩展名的原因。
pub fn decode(path: &Path) -> AppResult<DecodedImage> {
    decode_with(
        ImageReader::open(path)
            .map_err(|error| AppError::message(format!("无法打开图片：{error}")))?,
        path.extension().and_then(|value| value.to_str()),
    )
}

/// 从已读进内存的字节解码；调用方已经为色彩配置判断或元数据搬运读过一次文件时用它。
///
/// `format_hint` 是数据库里的文件扩展名，只在魔数认不出来时兜底（TGA 依赖它）。
pub fn decode_bytes(bytes: &[u8], format_hint: Option<&str>) -> AppResult<DecodedImage> {
    decode_with(ImageReader::new(std::io::Cursor::new(bytes)), format_hint)
}

fn decode_with<R: std::io::BufRead + std::io::Seek>(
    reader: ImageReader<R>,
    format_hint: Option<&str>,
) -> AppResult<DecodedImage> {
    let mut reader = reader
        .with_guessed_format()
        .map_err(|error| AppError::message(format!("无法识别图片格式：{error}")))?;

    if reader.format().is_none()
        && let Some(format) = format_hint.and_then(ImageFormat::from_extension)
    {
        reader.set_format(format);
    }

    let format = reader
        .format()
        .ok_or_else(|| AppError::message("无法识别图片格式"))?;
    let format_name = format_name(format)
        .ok_or_else(|| AppError::message(format!("不支持的图片格式：{format:?}")))?;

    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE);
    limits.max_image_height = Some(MAX_DECODE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);

    let mut decoder = reader
        .into_decoder()
        .map_err(|error| AppError::message(format!("图片解码器初始化失败：{error}")))?;

    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height)?;

    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|error| AppError::message(format!("图片解码失败：{error}")))?;
    image.apply_orientation(orientation);

    Ok(DecodedImage {
        has_alpha: image.color().has_alpha(),
        width: image.width(),
        height: image.height(),
        format: format_name,
        image,
    })
}

pub fn validate_dimensions(width: u32, height: u32) -> AppResult<()> {
    if width == 0 || height == 0 {
        return Err(AppError::message("图片尺寸无效"));
    }
    if width > MAX_DECODE_EDGE || height > MAX_DECODE_EDGE {
        return Err(AppError::message(format!(
            "图片尺寸 {width}×{height} 超过单边上限 {MAX_DECODE_EDGE}"
        )));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_DECODE_PIXELS {
        return Err(AppError::message(format!(
            "图片尺寸 {width}×{height} 超过解码上限 {MAX_DECODE_PIXELS} 像素"
        )));
    }
    Ok(())
}

fn format_name(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Jpeg => Some("jpeg"),
        ImageFormat::Png => Some("png"),
        ImageFormat::Gif => Some("gif"),
        ImageFormat::WebP => Some("webp"),
        ImageFormat::Tiff => Some("tiff"),
        ImageFormat::Tga => Some("tga"),
        ImageFormat::Bmp => Some("bmp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{GenericImageView, ImageFormat, Rgb, RgbImage};
    use tempfile::tempdir;

    use super::*;

    fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let image = RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 120])
        });
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, ImageFormat::Jpeg)
            .unwrap();
        let jpeg = encoded.into_inner();

        let mut exif = Vec::new();
        exif.extend_from_slice(b"Exif\0\0");
        exif.extend_from_slice(&[0x49, 0x49, 0x2a, 0x00]); // little-endian TIFF
        exif.extend_from_slice(&8_u32.to_le_bytes()); // IFD0 offset
        exif.extend_from_slice(&1_u16.to_le_bytes()); // one entry
        exif.extend_from_slice(&0x0112_u16.to_le_bytes()); // Orientation
        exif.extend_from_slice(&3_u16.to_le_bytes()); // SHORT
        exif.extend_from_slice(&1_u32.to_le_bytes());
        exif.extend_from_slice(&orientation.to_le_bytes());
        exif.extend_from_slice(&[0, 0]);
        exif.extend_from_slice(&0_u32.to_le_bytes()); // next IFD

        let mut app1 = Vec::new();
        app1.extend_from_slice(&[0xff, 0xe1]);
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);

        let mut output = Vec::with_capacity(jpeg.len() + app1.len());
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&app1);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    #[test]
    fn applies_exif_orientation_to_dimensions() {
        let temp = tempdir().unwrap();

        let normal = temp.path().join("normal.jpg");
        std::fs::write(&normal, jpeg_with_orientation(40, 20, 1)).unwrap();
        let decoded = decode(&normal).unwrap();
        assert_eq!((decoded.width, decoded.height), (40, 20));

        let rotated = temp.path().join("rotated.jpg");
        std::fs::write(&rotated, jpeg_with_orientation(40, 20, 6)).unwrap();
        let decoded = decode(&rotated).unwrap();
        assert_eq!((decoded.width, decoded.height), (20, 40));
    }

    #[test]
    fn decodes_png_and_reports_dimensions() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("sample.png");
        let mut image = RgbImage::new(40, 24);
        for pixel in image.pixels_mut() {
            *pixel = Rgb([10, 80, 140]);
        }
        DynamicImage::ImageRgb8(image).save(&path).unwrap();

        let decoded = decode(&path).unwrap();
        assert_eq!(decoded.format, "png");
        assert_eq!((decoded.width, decoded.height), (40, 24));
        assert!(!decoded.has_alpha);
    }

    #[test]
    fn decodes_tga_through_extension_fallback() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("sample.tga");
        let mut image = RgbImage::new(24, 16);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([(x % 256) as u8, (y % 256) as u8, 40]);
        }
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, ImageFormat::Tga)
            .unwrap();
        std::fs::write(&path, encoded.into_inner()).unwrap();

        // TGA 没有稳定魔数，只能靠扩展名兜底。
        let decoded = decode(&path).unwrap();
        assert_eq!(decoded.format, "tga");
        assert_eq!((decoded.width, decoded.height), (24, 16));
    }

    #[test]
    fn rejects_non_image_content() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("fake.png");
        std::fs::write(&path, b"not an image").unwrap();
        assert!(decode(&path).is_err());
        assert!(decode_bytes(b"not an image", Some("png")).is_err());
    }

    #[test]
    fn decodes_from_bytes_with_the_same_result_as_the_path() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("sample.png");
        let image = RgbImage::from_fn(32, 20, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        DynamicImage::ImageRgb8(image).save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();

        let from_path = decode(&path).unwrap();
        let from_bytes = decode_bytes(&bytes, Some("png")).unwrap();
        assert_eq!((from_bytes.width, from_bytes.height), (32, 20));
        assert_eq!(from_bytes.format, from_path.format);
        assert_eq!(from_bytes.has_alpha, from_path.has_alpha);
        assert_eq!(from_bytes.image.dimensions(), from_path.image.dimensions());
        assert_eq!(from_bytes.image.to_rgb8(), from_path.image.to_rgb8());
    }

    #[test]
    fn decodes_bytes_by_extension_hint_when_magic_is_missing() {
        let image = RgbImage::from_fn(24, 16, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 40]));
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, ImageFormat::Tga)
            .unwrap();
        let bytes = encoded.into_inner();

        let decoded = decode_bytes(&bytes, Some("tga")).unwrap();
        assert_eq!(decoded.format, "tga");
        assert_eq!((decoded.width, decoded.height), (24, 16));
        // 没有扩展名兜底时认不出 TGA，应当报错而不是猜一个格式。
        assert!(decode_bytes(&bytes, None).is_err());
    }

    #[test]
    fn applies_orientation_when_decoding_bytes() {
        let bytes = jpeg_with_orientation(40, 20, 6);
        let decoded = decode_bytes(&bytes, Some("jpg")).unwrap();
        assert_eq!((decoded.width, decoded.height), (20, 40));
    }

    #[test]
    fn rejects_oversized_dimensions_from_bytes() {
        // 伪造 PNG 头：IHDR 声明 70000×10，超过单边上限；解码器初始化时就会拒绝。
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&70_000_u32.to_be_bytes());
        ihdr.extend_from_slice(&10_u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        bytes.extend_from_slice(&(ihdr.len() as u32 - 4).to_be_bytes());
        bytes.extend_from_slice(&ihdr);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        assert!(decode_bytes(&bytes, Some("png")).is_err());
    }

    #[test]
    fn probes_dimensions_without_decoding_pixels() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("sample.png");
        RgbImage::new(320, 240).save(&path).unwrap();
        assert_eq!(probe_dimensions(&path).unwrap(), (320, 240));

        let tga = temp.path().join("sample.tga");
        let image = RgbImage::from_fn(24, 16, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 40]));
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, ImageFormat::Tga)
            .unwrap();
        std::fs::write(&tga, encoded.into_inner()).unwrap();
        assert_eq!(probe_dimensions(&tga).unwrap(), (24, 16));

        let fake = temp.path().join("fake.png");
        std::fs::write(&fake, b"not an image").unwrap();
        assert!(probe_dimensions(&fake).is_err());
    }

    #[test]
    fn enforces_pixel_limits() {
        assert!(validate_dimensions(0, 10).is_err());
        assert!(validate_dimensions(10, 10).is_ok());
        assert!(validate_dimensions(MAX_DECODE_EDGE + 1, 1).is_err());
        assert!(validate_dimensions(10_001, 10_000).is_err());
    }
}

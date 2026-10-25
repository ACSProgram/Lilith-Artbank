//! 图片空间优化的只读内核：判据、阶梯编码、画质比对与 JPEG 元数据搬运。
//!
//! 这里不读写数据库，也不发布文件；估算与执行共用同一套编码与裁决逻辑。

use image::{
    ColorType, DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder,
    codecs::jpeg::JpegEncoder, imageops::FilterType,
};
use serde::{Deserialize, Serialize};

use crate::app::error::{AppError, AppResult};

/// bpp 低于该值视为“已足够紧凑”，只做初判，不进入候选。
pub const BPP_THRESHOLD: f64 = 0.12;
/// 新文件必须小于原文件的这个比例才替换。
pub const MAX_SIZE_RATIO: f64 = 0.95;
/// 单通道差值超过该值的像素计入“局部坏点”。
pub const BAD_PIXEL_DELTA: u8 = 6;
/// WebP（VP8）单边上限；超过这个尺寸编码器会直接拒绝。
pub const WEBP_MAX_EDGE: u32 = 16_383;

/// 编码器存在量化误差，完全相同的图也可能只到 50-60 dB；用 99 表示“无差异”。
const PSNR_CAP: f64 = 99.0;

/// 阶梯质量；从档位起点开始只向上升，不向下降。
pub const QUALITY_LADDER: [u8; 6] = [75, 82, 85, 90, 95, 97];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum QualityTier {
    Extreme,
    #[default]
    High,
    Medium,
}

impl QualityTier {
    pub fn start_quality(self) -> u8 {
        match self {
            Self::Extreme => 95,
            Self::High => 85,
            Self::Medium => 75,
        }
    }

    /// 有损源是在已有损失上再压，无损源是从零开始转有损；两者的底线不同。
    pub fn psnr_floor(self, lossless_source: bool) -> f64 {
        match (self, lossless_source) {
            (Self::Extreme, false) => 45.0,
            (Self::Extreme, true) => 42.0,
            (Self::High, false) => 40.0,
            (Self::High, true) => 36.0,
            (Self::Medium, false) => 36.0,
            (Self::Medium, true) => 32.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OptimizeFormat {
    #[default]
    Jpeg,
    WebP,
}

impl OptimizeFormat {
    /// 新文件的扩展名；原图按内容哈希命名，扩展名跟着输出格式走。
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
        }
    }

    /// 写进 `images.format` 的格式名。
    pub fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::WebP => "webp",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OptimizeOptions {
    pub tier: QualityTier,
    pub output_format: OptimizeFormat,
    /// 长边上限；`None` 表示不缩放。
    pub max_long_edge: Option<u32>,
    /// 默认跳过已有 `optimized_at` 标记的图片，避免对同一张图反复有损压缩。
    pub skip_optimized: bool,
    pub regenerate_thumbnails: bool,
    pub recompute_phash: bool,
    pub recompute_colors: bool,
}

impl Default for OptimizeOptions {
    fn default() -> Self {
        Self {
            tier: QualityTier::High,
            output_format: OptimizeFormat::Jpeg,
            max_long_edge: None,
            skip_optimized: true,
            regenerate_thumbnails: false,
            recompute_phash: false,
            recompute_colors: false,
        }
    }
}

impl OptimizeOptions {
    pub fn validate(&self) -> AppResult<()> {
        if let Some(edge) = self.max_long_edge
            && edge < 64
        {
            return Err(AppError::message("长边上限不能小于 64"));
        }
        Ok(())
    }
}

/// 优化判据需要的源信息；`bit_depth` 只有解码后才能确认，默认按 8 位处理。
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub format: String,
    pub file_size: i64,
    pub width: u32,
    pub height: u32,
    pub has_alpha: bool,
    /// 上次优化时间戳（毫秒）；0 表示没处理过。
    pub optimized_at: i64,
    pub bit_depth: u8,
}

impl SourceInfo {
    pub fn bpp(&self) -> f64 {
        let pixels = u64::from(self.width).saturating_mul(u64::from(self.height));
        if pixels == 0 {
            return 0.0;
        }
        self.file_size.max(0) as f64 * 8.0 / pixels as f64
    }

    pub fn lossless_source(&self) -> bool {
        !matches!(self.format.as_str(), "jpeg" | "jpg" | "webp")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkipReason {
    /// bpp 太低，压不出空间。
    BppLow,
    /// 已经优化过。
    AlreadyOptimized,
    /// 有透明通道，而输出格式装不下。
    Transparent,
    /// 单边超过 WebP 的 16383 上限，输出 WebP 时无法编码。
    TooLargeForWebp,
    /// 源文件带非 sRGB 的 ICC 配置，而输出格式装不下它；丢掉会让颜色变样。
    ColorProfile,
    /// 16 位源，降到 8 位会损失色阶。
    SixteenBit,
    /// 阶梯用尽仍达不到 PSNR 底线。
    QualityNotMet,
    /// 达到画质底线，但省不到 5%。
    NotSmallEnough,
}

#[derive(Debug, Clone, Copy)]
pub struct OptimizePlan {
    pub start_quality: u8,
    pub psnr_floor: f64,
}

/// 只依赖数据库列的前置判据：bpp、已优化、透明。
///
/// 执行与估算共用这一段，保证“估算里被跳过的图，执行时也跳过”。
/// 16 位源要解码后才知道，不在这里判断。
pub fn prefilter(source: &SourceInfo, options: &OptimizeOptions) -> Result<(), SkipReason> {
    if source.bpp() < BPP_THRESHOLD {
        return Err(SkipReason::BppLow);
    }
    if options.skip_optimized && source.optimized_at > 0 {
        return Err(SkipReason::AlreadyOptimized);
    }
    if source.has_alpha && options.output_format == OptimizeFormat::Jpeg {
        return Err(SkipReason::Transparent);
    }
    // 长边上限不高于 16383 时，`prepare` 一定会把尺寸压进 WebP 的范围内。
    let resized_into_range = options
        .max_long_edge
        .is_some_and(|edge| edge <= WEBP_MAX_EDGE);
    if options.output_format == OptimizeFormat::WebP
        && !resized_into_range
        && (source.width > WEBP_MAX_EDGE || source.height > WEBP_MAX_EDGE)
    {
        return Err(SkipReason::TooLargeForWebp);
    }
    Ok(())
}

/// 执行前判据；画质与“省得太少”两条要在编码后才知道。
pub fn plan(source: &SourceInfo, options: &OptimizeOptions) -> Result<OptimizePlan, SkipReason> {
    prefilter(source, options)?;
    if source.bit_depth > 8 {
        return Err(SkipReason::SixteenBit);
    }
    Ok(OptimizePlan {
        start_quality: options.tier.start_quality(),
        psnr_floor: options.tier.psnr_floor(source.lossless_source()),
    })
}

/// 输出格式是否保留源文件的 ICC 配置：只有 JPEG → JPEG 会搬运。
fn keeps_color_profile(source_format: &str, output: OptimizeFormat) -> bool {
    output == OptimizeFormat::Jpeg && matches!(source_format, "jpeg" | "jpg")
}

/// 输出装不下源文件的非 sRGB 配置时跳过，避免颜色变样。
///
/// 只看配置里的 `desc` 名称：名字带 sRGB 才放行，其余（含认不出来的）一律按
/// “会变色”处理——宁可少压一张，也不让颜色变样。
pub fn color_profile_guard(
    bytes: &[u8],
    source_format: &str,
    options: &OptimizeOptions,
) -> Result<(), SkipReason> {
    if keeps_color_profile(source_format, options.output_format) {
        return Ok(());
    }
    match color_profile_is_srgb(bytes) {
        Some(false) => Err(SkipReason::ColorProfile),
        _ => Ok(()),
    }
}

/// 源文件嵌入的 ICC 配置是否为 sRGB；`None` 表示没有嵌入配置。
pub fn color_profile_is_srgb(bytes: &[u8]) -> Option<bool> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return jpeg_icc(bytes).map(|profile| icc_is_srgb(&profile));
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png_icc_name(bytes).map(|name| name_is_srgb(&name));
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return webp_icc(bytes).map(icc_is_srgb);
    }
    None
}

fn name_is_srgb(name: &str) -> bool {
    name.to_ascii_lowercase().contains("srgb")
}

fn icc_is_srgb(profile: &[u8]) -> bool {
    icc_profile_name(profile).is_some_and(|name| name_is_srgb(&name))
}

/// ICC 配置里 `desc`（或 `mluc`）标签的说明文字。
fn icc_profile_name(profile: &[u8]) -> Option<String> {
    if profile.len() < 132 {
        return None;
    }
    let count = usize::try_from(read_u32(profile, 128, false)?).ok()?;
    for index in 0..count.min(64) {
        let entry = 132 + index * 12;
        if profile.get(entry..entry + 4)? != b"desc" {
            continue;
        }
        let offset = usize::try_from(read_u32(profile, entry + 4, false)?).ok()?;
        match profile.get(offset..offset + 4)? {
            b"desc" => {
                let length = usize::try_from(read_u32(profile, offset + 8, false)?).ok()?;
                let text = profile.get(offset + 12..offset + 12 + length.saturating_sub(1))?;
                return Some(
                    String::from_utf8_lossy(text)
                        .trim_end_matches('\0')
                        .to_owned(),
                );
            }
            b"mluc" => {
                let record = offset + 16;
                let length = usize::try_from(read_u32(profile, record + 4, false)?).ok()?;
                let start = usize::try_from(read_u32(profile, record + 8, false)?).ok()?;
                let raw = profile.get(offset + start..offset + start + length)?;
                let mut text = String::new();
                for pair in raw.chunks_exact(2) {
                    let unit = u16::from_be_bytes([pair[0], pair[1]]);
                    if unit == 0 {
                        break;
                    }
                    text.push(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'));
                }
                return Some(text);
            }
            _ => return None,
        }
    }
    None
}

/// JPEG 的 ICC 可能分多段存放，按序号拼起来。
fn jpeg_icc(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut parts: Vec<(u8, &[u8])> = Vec::new();
    let mut offset = 2;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xFF {
            break;
        }
        while offset < bytes.len() && bytes[offset] == 0xFF {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        let length = usize::from(u16::from_be_bytes([
            *bytes.get(offset)?,
            *bytes.get(offset + 1)?,
        ]));
        if length < 2 || offset + length > bytes.len() {
            break;
        }
        let payload = &bytes[offset + 2..offset + length];
        if marker == 0xE2 && payload.starts_with(b"ICC_PROFILE\0") && payload.len() > 14 {
            parts.push((payload[12], &payload[14..]));
        }
        offset += length;
    }
    if parts.is_empty() {
        return None;
    }
    parts.sort_by_key(|part| part.0);
    let mut profile = Vec::with_capacity(parts.iter().map(|part| part.1.len()).sum());
    for (_, part) in parts {
        profile.extend_from_slice(part);
    }
    Some(profile)
}

/// PNG 的 `iCCP` 里配置是压缩过的；这里只取前面的配置名，不解压。
fn png_icc_name(bytes: &[u8]) -> Option<String> {
    let mut offset = 8;
    while offset + 8 <= bytes.len() {
        let length = u32::from_be_bytes([
            *bytes.get(offset)?,
            *bytes.get(offset + 1)?,
            *bytes.get(offset + 2)?,
            *bytes.get(offset + 3)?,
        ]) as usize;
        let kind = bytes.get(offset + 4..offset + 8)?;
        if kind == b"iCCP" {
            let data = bytes.get(offset + 8..offset + 8 + length)?;
            let end = data.iter().position(|byte| *byte == 0)?;
            return Some(String::from_utf8_lossy(&data[..end]).into_owned());
        }
        if kind == b"IDAT" || kind == b"IEND" {
            break;
        }
        offset = offset.checked_add(12 + length)?;
    }
    None
}

fn webp_icc(bytes: &[u8]) -> Option<&[u8]> {
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let kind = bytes.get(offset..offset + 4)?;
        let length = u32::from_le_bytes([
            *bytes.get(offset + 4)?,
            *bytes.get(offset + 5)?,
            *bytes.get(offset + 6)?,
            *bytes.get(offset + 7)?,
        ]) as usize;
        if kind == b"ICCP" {
            return bytes.get(offset + 8..offset + 8 + length);
        }
        offset = offset.checked_add(8 + length + (length & 1))?;
    }
    None
}

/// 解码后的位深；16 位源降到 8 位会损失色阶，执行与估算都要用同一条判断。
pub fn bit_depth(image: &DynamicImage) -> u8 {
    match image.color() {
        ColorType::L16 | ColorType::La16 | ColorType::Rgb16 | ColorType::Rgba16 => 16,
        _ => 8,
    }
}

/// 从起点开始的质量序列；起点不在标准阶梯里时先试起点本身。
pub fn quality_ladder(start: u8) -> Vec<u8> {
    let start = start.clamp(1, 100);
    let mut qualities: Vec<u8> = QUALITY_LADDER
        .iter()
        .copied()
        .filter(|quality| *quality >= start)
        .collect();
    if qualities.first() != Some(&start) {
        qualities.insert(0, start);
    }
    qualities
}

#[derive(Debug, Clone)]
pub struct EncodedCandidate {
    pub bytes: Vec<u8>,
    pub quality: u8,
    /// 单通道差值超过 6 的像素占比；估算用它做抽样诊断，不参与裁决。
    pub bad_pixel_ratio: f64,
}

#[derive(Debug, Clone)]
pub enum LadderOutcome {
    Encoded(EncodedCandidate),
    Skipped(SkipReason),
}

/// 重采样到长边上限；不放大。
pub fn prepare(image: DynamicImage, max_long_edge: Option<u32>) -> DynamicImage {
    let Some(edge) = max_long_edge else {
        return image;
    };
    let width = image.width();
    let height = image.height();
    if width.max(height) <= edge {
        return image;
    }
    let scale = f64::from(edge) / f64::from(width.max(height));
    let target_width = ((f64::from(width) * scale).round() as u32).max(1);
    let target_height = ((f64::from(height) * scale).round() as u32).max(1);
    image.resize(target_width, target_height, FilterType::Lanczos3)
}

/// 阶梯编码：达到 PSNR 底线后，只有确实小 5% 才返回结果。
///
/// `prepared` 应已经过 `prepare`，`original_bytes` 是当前原文件大小。
/// `source_jpeg` 只在“JPEG 输入 → JPEG 输出”时提供，用于搬运 EXIF/ICC。
pub fn encode_ladder(
    prepared: &DynamicImage,
    plan: &OptimizePlan,
    options: &OptimizeOptions,
    original_bytes: u64,
    source_jpeg: Option<&[u8]>,
) -> AppResult<LadderOutcome> {
    options.validate()?;
    if prepared.color().has_alpha() && options.output_format == OptimizeFormat::Jpeg {
        return Ok(LadderOutcome::Skipped(SkipReason::Transparent));
    }
    for quality in quality_ladder(plan.start_quality) {
        let bytes = encode(
            prepared,
            options.output_format,
            quality,
            source_jpeg.filter(|_| options.output_format == OptimizeFormat::Jpeg),
        )?;
        let encoded = image::load_from_memory(&bytes)
            .map_err(|error| AppError::message(format!("无法回读编码结果：{error}")))?;
        let metrics = compare(prepared, &encoded)?;
        if metrics.psnr < plan.psnr_floor {
            continue;
        }
        if !is_smaller_enough(original_bytes, bytes.len() as u64) {
            return Ok(LadderOutcome::Skipped(SkipReason::NotSmallEnough));
        }
        return Ok(LadderOutcome::Encoded(EncodedCandidate {
            bytes,
            quality,
            bad_pixel_ratio: metrics.bad_pixel_ratio,
        }));
    }
    Ok(LadderOutcome::Skipped(SkipReason::QualityNotMet))
}

pub fn is_smaller_enough(original: u64, encoded: u64) -> bool {
    encoded.saturating_mul(100) < original.saturating_mul(95)
}

/// 编码一次；输出 JPEG 时搬运源 JPEG 的 EXIF（去掉 Orientation）与 ICC。
pub fn encode(
    image: &DynamicImage,
    format: OptimizeFormat,
    quality: u8,
    source_jpeg: Option<&[u8]>,
) -> AppResult<Vec<u8>> {
    if !(1..=100).contains(&quality) {
        return Err(AppError::message("输出质量必须在 1-100 之间"));
    }
    match format {
        OptimizeFormat::Jpeg => {
            let rgb = image.to_rgb8();
            let mut bytes = Vec::new();
            JpegEncoder::new_with_quality(&mut bytes, quality).write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                ExtendedColorType::Rgb8,
            )?;
            Ok(match source_jpeg {
                Some(source) => copy_jpeg_metadata(source, &bytes),
                None => bytes,
            })
        }
        OptimizeFormat::WebP => {
            let (width, height) = (image.width(), image.height());
            if width > WEBP_MAX_EDGE || height > WEBP_MAX_EDGE {
                return Err(AppError::message(format!(
                    "图片尺寸 {width}×{height} 超过 WebP 单边上限 {WEBP_MAX_EDGE}"
                )));
            }
            // `webp` 的 `encode()` 内部是 `unwrap`：编码器拒绝时会把错误变成 panic。
            // 这里兜住，让异常落进失败清单，而不是把整个任务线程带走。
            let encoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if image.color().has_alpha() {
                    let rgba = image.to_rgba8();
                    webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
                        .encode(f32::from(quality))
                } else {
                    let rgb = image.to_rgb8();
                    webp::Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height())
                        .encode(f32::from(quality))
                }
            }))
            .map_err(|_| AppError::message("WebP 编码器拒绝了这张图片"))?;
            if encoded.is_empty() {
                return Err(AppError::message("WebP 编码结果为空"));
            }
            Ok(encoded.to_vec())
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QualityMetrics {
    pub psnr: f64,
    /// 单通道差值超过 6 的像素占比；用于观察色带，不参与 v1 裁决。
    pub bad_pixel_ratio: f64,
}

/// 逐像素比对；一次遍历同时算 MSE 与局部坏点。
pub fn compare(original: &DynamicImage, encoded: &DynamicImage) -> AppResult<QualityMetrics> {
    if original.dimensions() != encoded.dimensions() {
        return Err(AppError::message("画质比对的两张图尺寸不一致"));
    }
    let rgba = original.color().has_alpha() || encoded.color().has_alpha();
    let (source, target) = if rgba {
        (original.to_rgba8(), encoded.to_rgba8())
    } else {
        let source = original.to_rgb8();
        let target = encoded.to_rgb8();
        return Ok(metrics(source.as_raw(), target.as_raw(), 3));
    };
    Ok(metrics(source.as_raw(), target.as_raw(), 4))
}

fn metrics(source: &[u8], target: &[u8], channels: usize) -> QualityMetrics {
    let mut squared_error = 0.0_f64;
    let mut bad_pixels = 0_u64;
    let mut pixels = 0_u64;
    for (left, right) in source
        .chunks_exact(channels)
        .zip(target.chunks_exact(channels))
    {
        pixels += 1;
        let mut bad = false;
        for channel in 0..channels {
            let difference = i32::from(left[channel]) - i32::from(right[channel]);
            squared_error += f64::from(difference * difference);
            if difference.unsigned_abs() > u32::from(BAD_PIXEL_DELTA) {
                bad = true;
            }
        }
        if bad {
            bad_pixels += 1;
        }
    }
    let samples = (pixels as usize * channels).max(1) as f64;
    let mse = squared_error / samples;
    let psnr = if mse <= f64::EPSILON {
        PSNR_CAP
    } else {
        (10.0 * (255.0 * 255.0 / mse).log10()).min(PSNR_CAP)
    };
    QualityMetrics {
        psnr,
        bad_pixel_ratio: bad_pixels as f64 / pixels.max(1) as f64,
    }
}

/// 把源 JPEG 的 EXIF（去 Orientation）与 ICC 段插到新 JPEG 的 SOI 之后。
///
/// 只处理 APP1（Exif）与 APP2（ICC_PROFILE）；其它段不搬。源文件不是 JPEG 或
/// 结构不完整时原样返回编码结果，不冒险输出可能损坏的元数据。
pub fn copy_jpeg_metadata(source: &[u8], encoded: &[u8]) -> Vec<u8> {
    let segments = jpeg_metadata_segments(source);
    if segments.is_empty() || encoded.len() < 2 {
        return encoded.to_vec();
    }
    let mut output =
        Vec::with_capacity(encoded.len() + segments.iter().map(Vec::len).sum::<usize>());
    output.extend_from_slice(&encoded[..2]);
    for segment in &segments {
        output.extend_from_slice(segment);
    }
    output.extend_from_slice(&encoded[2..]);
    output
}

fn jpeg_metadata_segments(source: &[u8]) -> Vec<Vec<u8>> {
    if source.len() < 4 || source[..2] != [0xFF, 0xD8] {
        return Vec::new();
    }
    let mut segments = Vec::new();
    let mut offset = 2;
    while offset + 4 <= source.len() {
        if source[offset] != 0xFF {
            break;
        }
        while offset < source.len() && source[offset] == 0xFF {
            offset += 1;
        }
        if offset >= source.len() {
            break;
        }
        let marker = source[offset];
        offset += 1;
        if marker == 0xD8 {
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if offset + 2 > source.len() {
            break;
        }
        let length = usize::from(u16::from_be_bytes([source[offset], source[offset + 1]]));
        if length < 2 || offset + length > source.len() {
            break;
        }
        let payload = &source[offset + 2..offset + length];
        if marker == 0xE1 && payload.starts_with(b"Exif\0\0") {
            if let Some(stripped) = strip_orientation(payload)
                && stripped.len() <= 65_533
            {
                segments.push(segment(0xE1, &stripped));
            }
        } else if marker == 0xE2 && payload.starts_with(b"ICC_PROFILE\0") {
            segments.push(segment(0xE2, payload));
        }
        offset += length;
    }
    segments
}

fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(payload.len() + 4);
    output.extend_from_slice(&[0xFF, marker]);
    output.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    output.extend_from_slice(payload);
    output
}

/// 从 APP1 的 Exif 负载里删掉 IFD0 的 Orientation 标签。
///
/// 只压缩 IFD0 本身：后续条目整体左移 12 字节，next-IFD 指针与所有指向 IFD 外部
/// 数据的偏移都不变；释放出的 12 字节留在原段尾。段长度保持不变。
fn strip_orientation(payload: &[u8]) -> Option<Vec<u8>> {
    const EXIF_HEADER: &[u8] = b"Exif\0\0";
    if payload.len() < EXIF_HEADER.len() + 8 || !payload.starts_with(EXIF_HEADER) {
        return None;
    }
    let tiff_start = EXIF_HEADER.len();
    let tiff = &payload[tiff_start..];
    let little = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if read_u16(tiff, 2, little)? != 42 {
        return None;
    }
    let ifd_offset = usize::try_from(read_u32(tiff, 4, little)?).ok()?;
    if ifd_offset.checked_add(2)? > tiff.len() {
        return None;
    }
    let count = usize::from(read_u16(tiff, ifd_offset, little)?);
    let entries_start = ifd_offset + 2;
    let ifd_end = entries_start.checked_add(count.checked_mul(12)?)?;
    if ifd_end.checked_add(4)? > tiff.len() {
        return None;
    }
    let mut removed = None;
    for index in 0..count {
        let entry = entries_start + index * 12;
        if read_u16(tiff, entry, little)? == 0x0112 {
            removed = Some(index);
            break;
        }
    }
    let Some(index) = removed else {
        return Some(payload.to_vec());
    };

    let mut output = payload.to_vec();
    let entry_start = tiff_start + entries_start + index * 12;
    let copy_from = entry_start + 12;
    let copy_to = tiff_start + ifd_end + 4;
    if copy_from > copy_to {
        return None;
    }
    output.copy_within(copy_from..copy_to, entry_start);
    for byte in &mut output[copy_to - 12..copy_to] {
        *byte = 0;
    }
    write_u16(
        &mut output[tiff_start + ifd_offset..],
        u16::try_from(count - 1).ok()?,
        little,
    );
    Some(output)
}

fn read_u16(data: &[u8], offset: usize, little: bool) -> Option<u16> {
    let bytes: [u8; 2] = data.get(offset..offset + 2)?.try_into().ok()?;
    Some(if little {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    })
}

fn read_u32(data: &[u8], offset: usize, little: bool) -> Option<u32> {
    let bytes: [u8; 4] = data.get(offset..offset + 4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}

fn write_u16(data: &mut [u8], value: u16, little: bool) {
    let bytes = if little {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    data[..2].copy_from_slice(&bytes);
}

/// 判断图片是否带非默认 EXIF 方向；只在测试里用来验证搬运结果。
#[cfg(test)]
pub fn has_orientation(data: &[u8]) -> bool {
    use std::io::Cursor;

    use image::ImageDecoder;

    let Ok(reader) = image::ImageReader::new(Cursor::new(data)).with_guessed_format() else {
        return false;
    };
    let Ok(mut decoder) = reader.into_decoder() else {
        return false;
    };
    decoder
        .orientation()
        .is_ok_and(|orientation| orientation != image::metadata::Orientation::NoTransforms)
}

#[cfg(test)]
mod tests {
    use image::{Rgb, RgbImage};

    use super::*;

    fn source(format: &str, bytes: i64, width: u32, height: u32) -> SourceInfo {
        SourceInfo {
            format: format.into(),
            file_size: bytes,
            width,
            height,
            has_alpha: false,
            optimized_at: 0,
            bit_depth: 8,
        }
    }

    fn textured(width: u32, height: u32) -> DynamicImage {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let value = ((x * 37 + y * 91 + (x ^ y) * 13) % 256) as u8;
            *pixel = Rgb([
                value,
                value.wrapping_mul(3).wrapping_add((x % 17) as u8),
                value.wrapping_mul(7).wrapping_add((y % 23) as u8),
            ]);
        }
        DynamicImage::ImageRgb8(image)
    }

    fn encode_with_quality(image: &DynamicImage, quality: u8) -> Vec<u8> {
        encode(image, OptimizeFormat::Jpeg, quality, None).unwrap()
    }

    fn psnr_at(image: &DynamicImage, quality: u8) -> f64 {
        let bytes = encode_with_quality(image, quality);
        let decoded = image::load_from_memory(&bytes).unwrap();
        compare(image, &decoded).unwrap().psnr
    }

    fn jpeg_with_metadata(orientation: u16, little: bool) -> Vec<u8> {
        let image = textured(40, 24);
        let jpeg = encode_with_quality(&image, 90);

        // IFD0 里放两个条目：Orientation 与 Make，验证删条目后另一个条目仍可读。
        let mut exif = Vec::new();
        exif.extend_from_slice(b"Exif\0\0");
        if little {
            exif.extend_from_slice(b"II");
            exif.extend_from_slice(&42_u16.to_le_bytes());
            exif.extend_from_slice(&8_u32.to_le_bytes());
            exif.extend_from_slice(&2_u16.to_le_bytes());
            exif.extend_from_slice(&0x0112_u16.to_le_bytes());
            exif.extend_from_slice(&3_u16.to_le_bytes());
            exif.extend_from_slice(&1_u32.to_le_bytes());
            exif.extend_from_slice(&orientation.to_le_bytes());
            exif.extend_from_slice(&[0, 0]);
            exif.extend_from_slice(&0x010F_u16.to_le_bytes());
            exif.extend_from_slice(&2_u16.to_le_bytes());
            exif.extend_from_slice(&3_u32.to_le_bytes());
            exif.extend_from_slice(b"AC\0");
            exif.push(0);
            exif.extend_from_slice(&0_u32.to_le_bytes());
        } else {
            exif.extend_from_slice(b"MM");
            exif.extend_from_slice(&42_u16.to_be_bytes());
            exif.extend_from_slice(&8_u32.to_be_bytes());
            exif.extend_from_slice(&2_u16.to_be_bytes());
            exif.extend_from_slice(&0x0112_u16.to_be_bytes());
            exif.extend_from_slice(&3_u16.to_be_bytes());
            exif.extend_from_slice(&1_u32.to_be_bytes());
            exif.extend_from_slice(&orientation.to_be_bytes());
            exif.extend_from_slice(&[0, 0]);
            exif.extend_from_slice(&0x010F_u16.to_be_bytes());
            exif.extend_from_slice(&2_u16.to_be_bytes());
            exif.extend_from_slice(&3_u32.to_be_bytes());
            exif.extend_from_slice(b"AC\0");
            exif.push(0);
            exif.extend_from_slice(&0_u32.to_be_bytes());
        }
        let app1 = segment(0xE1, &exif);
        let mut icc = Vec::new();
        icc.extend_from_slice(b"ICC_PROFILE\0");
        icc.extend_from_slice(&[1, 1]);
        icc.extend_from_slice(b"fake-profile");
        let app2 = segment(0xE2, &icc);

        let mut output = Vec::new();
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&app1);
        output.extend_from_slice(&app2);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    fn has_app1_exif(data: &[u8]) -> bool {
        jpeg_metadata_segments(data)
            .iter()
            .any(|segment| segment[..2] == [0xFF, 0xE1])
    }

    #[test]
    fn plan_skips_low_bpp_optimized_alpha_and_16_bit() {
        let options = OptimizeOptions::default();
        assert_eq!(
            plan(&source("jpeg", 1_000, 4000, 4000), &options).unwrap_err(),
            SkipReason::BppLow
        );

        let mut optimized = source("jpeg", 8_000_000, 4000, 4000);
        optimized.optimized_at = 1;
        assert_eq!(
            plan(&optimized, &options).unwrap_err(),
            SkipReason::AlreadyOptimized
        );
        let reprocess = OptimizeOptions {
            skip_optimized: false,
            ..OptimizeOptions::default()
        };
        assert!(plan(&optimized, &reprocess).is_ok());

        let mut alpha = source("png", 8_000_000, 2000, 2000);
        alpha.has_alpha = true;
        assert_eq!(plan(&alpha, &options).unwrap_err(), SkipReason::Transparent);
        let webp = OptimizeOptions {
            output_format: OptimizeFormat::WebP,
            ..OptimizeOptions::default()
        };
        assert!(plan(&alpha, &webp).is_ok());

        let mut deep = source("tiff", 20_000_000, 2000, 2000);
        deep.bit_depth = 16;
        assert_eq!(plan(&deep, &options).unwrap_err(), SkipReason::SixteenBit);
    }

    fn icc_with_name(name: &str) -> Vec<u8> {
        let mut profile = vec![0u8; 128];
        profile.extend_from_slice(&1_u32.to_be_bytes());
        profile.extend_from_slice(b"desc");
        profile.extend_from_slice(&144_u32.to_be_bytes());
        profile.extend_from_slice(&0_u32.to_be_bytes());
        profile.extend_from_slice(b"desc");
        profile.extend_from_slice(&0_u32.to_be_bytes());
        profile.extend_from_slice(&(name.len() as u32 + 1).to_be_bytes());
        profile.extend_from_slice(name.as_bytes());
        profile.push(0);
        profile
    }

    fn jpeg_with_icc(profile: &[u8]) -> Vec<u8> {
        let base = encode_with_quality(&textured(24, 16), 90);
        let mut payload = Vec::new();
        payload.extend_from_slice(b"ICC_PROFILE\0");
        payload.extend_from_slice(&[1, 1]);
        payload.extend_from_slice(profile);
        let mut output = Vec::new();
        output.extend_from_slice(&base[..2]);
        output.extend_from_slice(&segment(0xE2, &payload));
        output.extend_from_slice(&base[2..]);
        output
    }

    #[test]
    fn skips_sources_whose_color_profile_would_be_dropped() {
        let webp = OptimizeOptions {
            output_format: OptimizeFormat::WebP,
            ..OptimizeOptions::default()
        };
        let srgb = jpeg_with_icc(&icc_with_name("sRGB IEC61966-2.1"));
        let adobe = jpeg_with_icc(&icc_with_name("Adobe RGB (1998)"));

        assert!(color_profile_guard(&srgb, "jpeg", &webp).is_ok());
        assert_eq!(
            color_profile_guard(&adobe, "jpeg", &webp),
            Err(SkipReason::ColorProfile)
        );
        // JPEG → JPEG 会搬运配置，不受影响。
        assert!(color_profile_guard(&adobe, "jpeg", &OptimizeOptions::default()).is_ok());
        // PNG 源转 JPEG 不搬运配置。
        assert_eq!(
            color_profile_guard(&adobe, "png", &OptimizeOptions::default()),
            Err(SkipReason::ColorProfile)
        );
        // 没有配置的图照常处理。
        let plain = encode_with_quality(&textured(24, 16), 90);
        assert!(color_profile_guard(&plain, "jpeg", &webp).is_ok());
    }

    #[test]
    fn webp_skips_images_beyond_its_edge_limit() {
        let webp = OptimizeOptions {
            output_format: OptimizeFormat::WebP,
            ..OptimizeOptions::default()
        };
        let tall = source("jpeg", 8_000_000, 391, 17_799);
        assert_eq!(plan(&tall, &webp).unwrap_err(), SkipReason::TooLargeForWebp);
        // 同一张图输出 JPEG 不受影响。
        assert!(plan(&tall, &OptimizeOptions::default()).is_ok());
        // 长边上限足够小时，缩放后必然在范围内，不该被前置判据拦掉。
        let resized = OptimizeOptions {
            max_long_edge: Some(4096),
            ..webp
        };
        assert!(plan(&tall, &resized).is_ok());

        // 编码层再兜一次：不 panic，返回可读错误。
        let image = DynamicImage::ImageRgb8(RgbImage::new(4, 16_400));
        let error = encode(&image, OptimizeFormat::WebP, 85, None).unwrap_err();
        assert!(error.to_string().contains("16383"), "{error}");
    }

    #[test]
    fn tier_uses_separate_floors_for_lossless_sources() {
        let high = plan(
            &source("jpeg", 8_000_000, 2000, 2000),
            &OptimizeOptions::default(),
        )
        .unwrap();
        assert_eq!(high.start_quality, 85);
        assert_eq!(high.psnr_floor, 40.0);

        let png = plan(
            &source("png", 8_000_000, 2000, 2000),
            &OptimizeOptions::default(),
        )
        .unwrap();
        assert_eq!(png.psnr_floor, 36.0);

        let extreme = OptimizeOptions {
            tier: QualityTier::Extreme,
            ..OptimizeOptions::default()
        };
        assert_eq!(
            plan(&source("jpeg", 8_000_000, 2000, 2000), &extreme)
                .unwrap()
                .start_quality,
            95
        );
        assert_eq!(
            plan(&source("png", 8_000_000, 2000, 2000), &extreme)
                .unwrap()
                .psnr_floor,
            42.0
        );
    }

    #[test]
    fn ladder_starts_at_tier_and_only_moves_up() {
        assert_eq!(quality_ladder(75), QUALITY_LADDER.to_vec());
        assert_eq!(quality_ladder(85), vec![85, 90, 95, 97]);
        assert_eq!(quality_ladder(95), vec![95, 97]);
        assert_eq!(quality_ladder(88), vec![88, 90, 95, 97]);
        assert_eq!(quality_ladder(98), vec![98]);
    }

    #[test]
    fn ladder_climbs_until_psnr_floor_is_met() {
        let image = textured(128, 96);
        let low = psnr_at(&image, 82);
        let high = psnr_at(&image, 85);
        assert!(low < high, "低质量 PSNR 应更低：{low} vs {high}");
        let plan = OptimizePlan {
            start_quality: 75,
            psnr_floor: (low + high) / 2.0,
        };
        let outcome =
            encode_ladder(&image, &plan, &OptimizeOptions::default(), 10_000_000, None).unwrap();
        match outcome {
            LadderOutcome::Encoded(candidate) => assert_eq!(candidate.quality, 85),
            LadderOutcome::Skipped(reason) => panic!("不应跳过：{reason:?}"),
        }
    }

    #[test]
    fn ladder_skips_when_quality_or_saving_is_not_enough() {
        let image = textured(96, 96);
        let impossible = OptimizePlan {
            start_quality: 75,
            psnr_floor: 100.0,
        };
        assert!(matches!(
            encode_ladder(
                &image,
                &impossible,
                &OptimizeOptions::default(),
                10_000_000,
                None
            )
            .unwrap(),
            LadderOutcome::Skipped(SkipReason::QualityNotMet)
        ));

        let easy = OptimizePlan {
            start_quality: 75,
            psnr_floor: 0.0,
        };
        assert!(matches!(
            encode_ladder(&image, &easy, &OptimizeOptions::default(), 100, None).unwrap(),
            LadderOutcome::Skipped(SkipReason::NotSmallEnough)
        ));
    }

    #[test]
    fn psnr_and_bad_pixel_ratio_are_computed_once() {
        let image = textured(32, 24);
        let metrics = compare(&image, &image.clone()).unwrap();
        assert_eq!(metrics.psnr, PSNR_CAP);
        assert_eq!(metrics.bad_pixel_ratio, 0.0);

        let mut changed = image.to_rgb8();
        changed.put_pixel(0, 0, Rgb([255, 255, 255]));
        let metrics = compare(&image, &DynamicImage::ImageRgb8(changed)).unwrap();
        assert!(metrics.psnr < PSNR_CAP);
        assert!(metrics.bad_pixel_ratio > 0.0);
    }

    #[test]
    fn prepare_resizes_long_edge_without_upscaling() {
        let image = textured(400, 200);
        let resized = prepare(image.clone(), Some(200));
        assert_eq!(resized.dimensions(), (200, 100));
        let unchanged = prepare(image.clone(), Some(800));
        assert_eq!(unchanged.dimensions(), (400, 200));
        let untouched = prepare(image, None);
        assert_eq!(untouched.dimensions(), (400, 200));
    }

    #[test]
    fn copies_exif_and_icc_but_strips_orientation() {
        for little in [true, false] {
            let source = jpeg_with_metadata(6, little);
            assert!(has_orientation(&source));
            let source_meta = jpeg_metadata_segments(&source);
            assert!(
                !source_meta.is_empty(),
                "测试源没有可搬运的元数据（little={little}）"
            );
            assert!(
                strip_orientation(&source_meta[0][4..]).is_some(),
                "Orientation 删除失败（little={little}）"
            );

            let image = image::load_from_memory(&source).unwrap();
            let encoded = encode(&image, OptimizeFormat::Jpeg, 90, Some(&source)).unwrap();
            assert!(
                !has_orientation(&encoded),
                "输出仍带方向标签（little={little}）"
            );
            assert!(has_app1_exif(&encoded));
            let metadata = jpeg_metadata_segments(&encoded);
            assert!(
                metadata.iter().any(|segment| segment[..2] == [0xFF, 0xE2]),
                "ICC 段未搬运"
            );

            let stripped = &metadata
                .iter()
                .find(|segment| segment[..2] == [0xFF, 0xE1])
                .unwrap()[4..];
            let tiff = &stripped[6..];
            let count = read_u16(tiff, 8, little).unwrap();
            assert_eq!(count, 1);
            assert_eq!(read_u16(tiff, 10, little).unwrap(), 0x010F);
        }
    }

    #[test]
    fn webp_output_does_not_copy_metadata() {
        let source = jpeg_with_metadata(6, true);
        let image = image::load_from_memory(&source).unwrap();
        let encoded = encode(&image, OptimizeFormat::WebP, 85, Some(&source)).unwrap();
        let decoded = image::load_from_memory(&encoded).unwrap();
        assert_eq!(decoded.dimensions(), image.dimensions());
        assert!(
            !jpeg_metadata_segments(&encoded)
                .iter()
                .any(|segment| segment[..2] == [0xFF, 0xE1])
        );
    }

    #[test]
    fn output_format_names_its_file_and_column() {
        assert_eq!(OptimizeFormat::Jpeg.extension(), "jpg");
        assert_eq!(OptimizeFormat::Jpeg.name(), "jpeg");
        assert_eq!(OptimizeFormat::WebP.extension(), "webp");
        assert_eq!(OptimizeFormat::WebP.name(), "webp");
    }

    #[test]
    fn prefilter_covers_database_columns_only() {
        let options = OptimizeOptions::default();
        let mut optimized = source("jpeg", 8_000_000, 4000, 4000);
        optimized.optimized_at = 1;
        assert_eq!(
            prefilter(&optimized, &options).unwrap_err(),
            SkipReason::AlreadyOptimized
        );

        let mut deep = source("tiff", 20_000_000, 2000, 2000);
        deep.bit_depth = 16;
        assert!(prefilter(&deep, &options).is_ok());
        assert_eq!(plan(&deep, &options).unwrap_err(), SkipReason::SixteenBit);
    }
}

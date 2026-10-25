use image::{
    DynamicImage, ExtendedColorType, ImageEncoder, Rgba, RgbaImage,
    codecs::jpeg::JpegEncoder,
    imageops::{self, FilterType},
};

use crate::app::error::{AppError, AppResult};

/// 拼图画布长边。格子数越多单格越小，1×1 时接近 1600。
pub const SHEET_LONG_EDGE: u32 = 1600;
/// 单个方向最多 5 格（5×5 = 25 张），编号也就到 `E5`。
pub const MAX_GRID: u8 = 5;
/// `detail` 直接解码原图，只允许 3×3 及以下。
pub const DETAIL_MAX_GRID: u8 = 3;
const GAP: u32 = 8;
const BACKGROUND: Rgba<u8> = Rgba([34, 34, 36, 255]);
const CELL_BACKGROUND: Rgba<u8> = Rgba([48, 48, 52, 255]);
const PLACEHOLDER: Rgba<u8> = Rgba([92, 92, 100, 255]);
const LABEL_BACKGROUND: Rgba<u8> = Rgba([12, 12, 14, 255]);
const LABEL_COLOR: Rgba<u8> = Rgba([240, 240, 240, 255]);
const JPEG_QUALITY: u8 = 88;

/// 把 `grid × grid` 张图片（可为缺图）合成一张带 A1…E5 编号的拼图。
///
/// 每一格等比 `contain`，不裁切；缺图格画占位，不阻塞整盘。
/// 图片由 `load` 按格子下标逐张提供，画完即释放，避免整盘原图同时驻留内存。
pub fn render(
    grid: u8,
    labels: &[String],
    mut load: impl FnMut(usize) -> Option<DynamicImage>,
) -> AppResult<Vec<u8>> {
    if !(1..=MAX_GRID).contains(&grid) {
        return Err(AppError::message(format!(
            "拼图格数必须在 1-{MAX_GRID} 之间"
        )));
    }
    let grid = u32::from(grid);
    let count = (grid * grid) as usize;
    if labels.len() != count {
        return Err(AppError::message("拼图格子映射与图片数量不一致"));
    }

    let cell = (SHEET_LONG_EDGE - GAP * (grid + 1)) / grid;
    let edge = cell * grid + GAP * (grid + 1);
    let mut canvas = RgbaImage::from_pixel(edge, edge, BACKGROUND);

    for (index, label) in labels.iter().enumerate() {
        let column = index as u32 % grid;
        let row = index as u32 / grid;
        let left = GAP + column * (cell + GAP);
        let top = GAP + row * (cell + GAP);
        fill_rect(
            &mut canvas,
            left as i64,
            top as i64,
            cell as i64,
            cell as i64,
            CELL_BACKGROUND,
        );

        if let Some(image) = load(index)
            && let Some(fitted) = fit_contain(&image, cell, cell)
        {
            let x = left + (cell - fitted.width()) / 2;
            let y = top + (cell - fitted.height()) / 2;
            imageops::overlay(&mut canvas, &fitted, i64::from(x), i64::from(y));
        } else {
            draw_missing_mark(&mut canvas, left, top, cell);
        }

        draw_label(&mut canvas, left + 6, top + 6, label);
    }

    let rgb = DynamicImage::ImageRgba8(canvas).to_rgb8();
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY).write_image(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        ExtendedColorType::Rgb8,
    )?;
    Ok(bytes)
}

/// 从缩略图包读出的 WebP/JPEG/PNG 字节。
pub fn decode_thumbnail(bytes: &[u8]) -> AppResult<DynamicImage> {
    image::load_from_memory(bytes)
        .map_err(|error| AppError::message(format!("缩略图解码失败：{error}")))
}

fn fit_contain(image: &DynamicImage, width: u32, height: u32) -> Option<RgbaImage> {
    let (source_width, source_height) = (image.width(), image.height());
    if source_width == 0 || source_height == 0 || width == 0 || height == 0 {
        return None;
    }
    let scale = (f64::from(width) / f64::from(source_width))
        .min(f64::from(height) / f64::from(source_height));
    let target_width = (f64::from(source_width) * scale).round().max(1.0) as u32;
    let target_height = (f64::from(source_height) * scale).round().max(1.0) as u32;
    Some(
        image
            .resize_exact(target_width, target_height, FilterType::Triangle)
            .to_rgba8(),
    )
}

fn fill_rect(image: &mut RgbaImage, x: i64, y: i64, width: i64, height: i64, color: Rgba<u8>) {
    for row in y..y + height {
        for column in x..x + width {
            if column >= 0
                && row >= 0
                && column < i64::from(image.width())
                && row < i64::from(image.height())
            {
                image.put_pixel(column as u32, row as u32, color);
            }
        }
    }
}

fn draw_missing_mark(image: &mut RgbaImage, left: u32, top: u32, size: u32) {
    let inset = size / 4;
    let start_x = i64::from(left + inset);
    let start_y = i64::from(top + inset);
    let end_x = i64::from(left + size - inset);
    let end_y = i64::from(top + size - inset);
    draw_line(image, start_x, start_y, end_x, end_y, PLACEHOLDER);
    draw_line(image, start_x, end_y, end_x, start_y, PLACEHOLDER);
}

fn draw_line(image: &mut RgbaImage, mut x0: i64, mut y0: i64, x1: i64, y1: i64, color: Rgba<u8>) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let step_x = if x0 < x1 { 1 } else { -1 };
    let step_y = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        if x0 >= 0 && y0 >= 0 && x0 < i64::from(image.width()) && y0 < i64::from(image.height()) {
            image.put_pixel(x0 as u32, y0 as u32, color);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x0 += step_x;
        }
        if doubled <= dx {
            error += dx;
            y0 += step_y;
        }
    }
}

fn draw_label(image: &mut RgbaImage, left: u32, top: u32, label: &str) {
    const SCALE: u32 = 3;
    const PADDING: u32 = 3;
    if label.is_empty() {
        return;
    }
    let width = label.chars().count() as u32 * 6 * SCALE - SCALE;
    let height = 7 * SCALE;
    fill_rect(
        image,
        i64::from(left - PADDING),
        i64::from(top - PADDING),
        i64::from(width + PADDING * 2),
        i64::from(height + PADDING * 2),
        LABEL_BACKGROUND,
    );
    let mut cursor = i64::from(left);
    for character in label.chars() {
        let Some(glyph) = glyph(character) else {
            cursor += i64::from(6 * SCALE);
            continue;
        };
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    fill_rect(
                        image,
                        cursor + i64::from(column * SCALE),
                        i64::from(top) + (row as u32 * SCALE) as i64,
                        i64::from(SCALE),
                        i64::from(SCALE),
                        LABEL_COLOR,
                    );
                }
            }
        }
        cursor += i64::from(6 * SCALE);
    }
}

/// 内置 5×7 位图字体，只包含编号会用到的 A–E 与 0–9。
fn glyph(character: char) -> Option<[u8; 7]> {
    Some(match character {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{GenericImageView, ImageFormat, Rgb, RgbImage};

    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 3]) -> DynamicImage {
        let mut image = RgbImage::new(width, height);
        for pixel in image.pixels_mut() {
            *pixel = Rgb(color);
        }
        DynamicImage::ImageRgb8(image)
    }

    fn labels(grid: usize) -> Vec<String> {
        (0..grid * grid)
            .map(|index| {
                let column = (b'A' + (index % grid) as u8) as char;
                let row = index / grid + 1;
                format!("{column}{row}")
            })
            .collect()
    }

    #[test]
    fn renders_square_sheet_with_expected_size() {
        let mut cells = [
            Some(solid(120, 80, [200, 40, 40])),
            Some(solid(60, 120, [40, 200, 40])),
            None,
            Some(solid(90, 90, [40, 40, 200])),
        ];
        let bytes = render(2, &labels(2), |index| cells[index].take()).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        let grid = 2_u32;
        let cell = (SHEET_LONG_EDGE - GAP * (grid + 1)) / grid;
        assert_eq!(
            decoded.dimensions(),
            (
                cell * grid + GAP * (grid + 1),
                cell * grid + GAP * (grid + 1)
            )
        );
    }

    #[test]
    fn missing_cell_draws_placeholder() {
        let bytes = render(1, &labels(1), |_| None).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap().to_rgb8();
        let center = decoded.get_pixel(decoded.width() / 2, decoded.height() / 2);
        assert!(center.0[0] > 70 && center.0[0] < 120, "缺图格应画占位标记");
    }

    #[test]
    fn rejects_invalid_grid_and_mapping() {
        assert!(render(0, &[], |_| None).is_err());
        assert!(render(6, &[], |_| None).is_err());
        assert!(render(2, &labels(1), |_| None).is_err());
    }

    #[test]
    fn decodes_thumbnails_from_memory() {
        let image = solid(32, 16, [10, 20, 30]);
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, ImageFormat::WebP).unwrap();
        let decoded = decode_thumbnail(encoded.get_ref()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (32, 16));
    }
}

use std::sync::OnceLock;

use image::{DynamicImage, imageops::FilterType};

pub const HASH_BYTES: usize = 32;

const SAMPLE: usize = 32;
const LOW: usize = 16;

/// 256 位感知哈希：32×32 灰度 → DCT-II → 左上 16×16 低频系数 → 中位数二值化。
///
/// DC 分量置 0 且不参与中位数计算；否则纯色图会把 256 位里的 1 位固定成 1，
/// 其余结构信息被压成 0。哈希只描述结构，不描述颜色。
pub fn compute(image: &DynamicImage) -> [u8; HASH_BYTES] {
    let gray = image.to_luma8();
    let resized =
        image::imageops::resize(&gray, SAMPLE as u32, SAMPLE as u32, FilterType::Triangle);

    let table = dct_table();
    let mut rows = [0.0_f32; SAMPLE * SAMPLE];
    for y in 0..SAMPLE {
        for u in 0..SAMPLE {
            let mut sum = 0.0_f32;
            for x in 0..SAMPLE {
                sum += f32::from(resized.get_pixel(x as u32, y as u32)[0]) * table[u * SAMPLE + x];
            }
            rows[y * SAMPLE + u] = sum;
        }
    }

    let mut low = [0.0_f32; LOW * LOW];
    for v in 0..LOW {
        for u in 0..LOW {
            let mut sum = 0.0_f32;
            for y in 0..SAMPLE {
                sum += rows[y * SAMPLE + u] * table[v * SAMPLE + y];
            }
            // 纯色图的 AC 系数只应剩浮点噪声；噪声经过中位数会随机点亮一半的位，
            // 所以低于量化阈值的系数直接归零。
            low[v * LOW + u] = if sum.abs() < 0.5 { 0.0 } else { sum };
        }
    }

    let mut sorted: Vec<f32> = low.iter().skip(1).copied().collect();
    sorted.sort_by(|left, right| left.total_cmp(right));
    let median = sorted[sorted.len() / 2];

    let mut hash = [0_u8; HASH_BYTES];
    for (index, value) in low.iter().enumerate() {
        if index > 0 && *value > median {
            hash[index / 8] |= 1 << (7 - (index % 8));
        }
    }
    hash
}

/// 按字节比较两个哈希；只用于测试与离线分析，线上走 `distance_words`。
#[cfg(test)]
pub fn distance(left: &[u8; HASH_BYTES], right: &[u8; HASH_BYTES]) -> u32 {
    left.iter()
        .zip(right.iter())
        .map(|(a, b)| (a ^ b).count_ones())
        .sum()
}

/// 把 32 字节哈希转成 4 个 u64，相似度扫描时一次比较 64 位。
pub fn words(hash: &[u8; HASH_BYTES]) -> [u64; 4] {
    let mut words = [0_u64; 4];
    for (index, chunk) in hash.chunks_exact(8).enumerate() {
        words[index] = u64::from_le_bytes(chunk.try_into().expect("hash chunk is 8 bytes"));
    }
    words
}

pub fn distance_words(left: &[u64; 4], right: &[u64; 4]) -> u32 {
    left.iter()
        .zip(right.iter())
        .map(|(a, b)| (a ^ b).count_ones())
        .sum()
}

fn dct_table() -> &'static [f32; SAMPLE * SAMPLE] {
    static TABLE: OnceLock<[f32; SAMPLE * SAMPLE]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0.0_f32; SAMPLE * SAMPLE];
        let scale = std::f64::consts::PI / (2.0 * SAMPLE as f64);
        for u in 0..SAMPLE {
            let alpha = if u == 0 {
                (1.0 / SAMPLE as f64).sqrt()
            } else {
                (2.0 / SAMPLE as f64).sqrt()
            };
            for x in 0..SAMPLE {
                table[u * SAMPLE + x] =
                    (alpha * (((2 * x + 1) as f64) * u as f64 * scale).cos()) as f32;
            }
        }
        table
    })
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, Rgb, RgbImage};

    use super::*;

    fn pattern(shift: u32) -> DynamicImage {
        let mut image = RgbImage::new(96, 96);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let inside = (x + shift) % 96 > 30 && (y + shift) % 96 < 60;
            *pixel = if inside {
                Rgb([230, 220, 200])
            } else {
                Rgb([30, 50, 90])
            };
        }
        DynamicImage::ImageRgb8(image)
    }

    #[test]
    fn identical_images_have_zero_distance() {
        let first = compute(&pattern(0));
        let second = compute(&pattern(0));
        assert_eq!(first, second);
        assert_eq!(distance(&first, &second), 0);
        assert_eq!(words(&first), words(&second));
    }

    #[test]
    fn different_structure_changes_hash() {
        let first = compute(&pattern(0));
        let second = compute(&pattern(21));
        assert!(distance(&first, &second) > 0);
    }

    #[test]
    fn hash_is_256_bits() {
        let hash = compute(&pattern(3));
        assert_eq!(hash.len(), HASH_BYTES);
        // 纯色图没有结构，所有位应为 0。
        let solid = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, Rgb([120, 90, 60])));
        assert_eq!(compute(&solid), [0_u8; HASH_BYTES]);
    }

    #[test]
    fn word_distance_matches_byte_distance() {
        let left = compute(&pattern(0));
        let right = compute(&pattern(21));
        assert_eq!(
            distance(&left, &right),
            distance_words(&words(&left), &words(&right))
        );
    }
}

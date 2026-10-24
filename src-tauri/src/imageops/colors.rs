use image::{DynamicImage, imageops::FilterType};

const SAMPLE_EDGE: u32 = 64;
const ITERATIONS: usize = 12;
const KMAX: usize = 8;

pub const COLORS4_BYTES: usize = 16;
pub const COLORS8_BYTES: usize = 32;

/// 4 色与 8 色主色，每项 `(r, g, b, weight)` 各 1 字节，权重合计 255。
///
/// 在 OKLab 里做确定性 k-means（固定种子、固定迭代上限）；不追求与旧 OpenCV
/// EMD 逐值一致，目标是稳定、可解释、支持加权距离。
pub fn dominant(image: &DynamicImage) -> ([u8; COLORS4_BYTES], [u8; COLORS8_BYTES]) {
    let rgb = image.to_rgb8();
    let sample = image::imageops::resize(&rgb, SAMPLE_EDGE, SAMPLE_EDGE, FilterType::Triangle);

    let labs: Vec<[f32; 3]> = sample
        .pixels()
        .map(|pixel| oklab_from_srgb(pixel[0], pixel[1], pixel[2]))
        .collect();

    let mut colors4 = [0_u8; COLORS4_BYTES];
    let mut colors8 = [0_u8; COLORS8_BYTES];
    write_palette(&labs, 4, &mut colors4);
    write_palette(&labs, KMAX, &mut colors8);
    (colors4, colors8)
}

fn write_palette(samples: &[[f32; 3]], k: usize, out: &mut [u8]) {
    let clusters = kmeans(samples, k);
    let palette = palette(samples.len(), &clusters);
    for (slot, (rgb, weight)) in out.chunks_exact_mut(4).zip(palette.iter()) {
        slot[0] = rgb[0];
        slot[1] = rgb[1];
        slot[2] = rgb[2];
        slot[3] = *weight;
    }
}

struct Cluster {
    centroid: [f32; 3],
    count: usize,
}

fn kmeans(samples: &[[f32; 3]], k: usize) -> Vec<Cluster> {
    if samples.is_empty() || k == 0 {
        return Vec::new();
    }
    let k = k.min(samples.len());
    let mut random = Rng(0x9e37_79b9_7f4a_7c15);
    let mut centroids = Vec::with_capacity(k);
    centroids.push(samples[(random.next_u64() % samples.len() as u64) as usize]);

    while centroids.len() < k {
        let mut distances = vec![0_f64; samples.len()];
        let mut total = 0_f64;
        for (index, sample) in samples.iter().enumerate() {
            let nearest = centroids
                .iter()
                .map(|centroid| distance_squared(sample, centroid))
                .fold(f64::INFINITY, f64::min);
            distances[index] = nearest;
            total += nearest;
        }
        let chosen = if total <= f64::EPSILON {
            let index = (random.next_u64() as usize) % samples.len();
            samples[index]
        } else {
            let mut target = random.next_f64() * total;
            let mut selected = samples.len() - 1;
            for (index, distance) in distances.iter().enumerate() {
                target -= distance;
                if target <= 0.0 {
                    selected = index;
                    break;
                }
            }
            samples[selected]
        };
        centroids.push(chosen);
    }

    let mut assignments = vec![usize::MAX; samples.len()];
    let mut counts = vec![0_usize; k];
    for _ in 0..ITERATIONS {
        let mut sums = vec![[0_f64; 3]; k];
        counts.fill(0);
        let mut changed = false;
        for (index, sample) in samples.iter().enumerate() {
            let mut best = 0;
            let mut best_distance = f64::INFINITY;
            for (cluster, centroid) in centroids.iter().enumerate() {
                let distance = distance_squared(sample, centroid);
                if distance < best_distance {
                    best_distance = distance;
                    best = cluster;
                }
            }
            if assignments[index] != best {
                assignments[index] = best;
                changed = true;
            }
            counts[best] += 1;
            for axis in 0..3 {
                sums[best][axis] += f64::from(sample[axis]);
            }
        }
        for cluster in 0..k {
            if counts[cluster] > 0 {
                let count = counts[cluster] as f64;
                centroids[cluster] = [
                    (sums[cluster][0] / count) as f32,
                    (sums[cluster][1] / count) as f32,
                    (sums[cluster][2] / count) as f32,
                ];
            }
        }
        if !changed {
            break;
        }
    }

    // 空簇（纯色图或退化初始化）保留初始样本作为代表色，权重为 0。
    centroids
        .into_iter()
        .zip(counts)
        .map(|(centroid, count)| Cluster { centroid, count })
        .collect()
}

fn palette(sample_count: usize, clusters: &[Cluster]) -> Vec<([u8; 3], u8)> {
    let mut entries: Vec<(usize, [u8; 3], f64)> = clusters
        .iter()
        .map(|cluster| {
            (
                cluster.count,
                srgb_from_oklab(cluster.centroid),
                cluster.count as f64 / sample_count.max(1) as f64,
            )
        })
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));

    let raw: Vec<f64> = entries.iter().map(|entry| entry.2 * 255.0).collect();
    let mut weights: Vec<u8> = raw.iter().map(|value| value.floor() as u8).collect();
    let assigned: u32 = weights.iter().map(|value| u32::from(*value)).sum();
    let mut remaining = 255_u32.saturating_sub(assigned);
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|left, right| {
        let left_fraction = raw[*left] - raw[*left].floor();
        let right_fraction = raw[*right] - raw[*right].floor();
        right_fraction.total_cmp(&left_fraction)
    });
    let mut cursor = 0;
    while remaining > 0 && !order.is_empty() {
        let index = order[cursor % order.len()];
        if weights[index] < 255 {
            weights[index] += 1;
            remaining -= 1;
        }
        cursor += 1;
    }

    entries
        .into_iter()
        .zip(weights)
        .map(|((_, rgb, _), weight)| (rgb, weight))
        .collect()
}

/// OKLab 预转换的主色，供常驻相似度索引复用，避免每次查询重复转换。
#[derive(Debug, Clone, Copy, Default)]
pub struct LabColor {
    pub l: f32,
    pub a: f32,
    pub b: f32,
    pub weight: f32,
}

/// 把 4/8 色 `(r, g, b, weight)` 记录预先转成 OKLab；权重归一到 0-1。
pub fn prepare_palette<const N: usize>(palette: &[u8]) -> [LabColor; N] {
    let mut out = [LabColor::default(); N];
    for (slot, entry) in out.iter_mut().zip(palette.chunks_exact(4)) {
        let [l, a, b] = oklab_from_srgb(entry[0], entry[1], entry[2]);
        *slot = LabColor {
            l,
            a,
            b,
            weight: f32::from(entry[3]) / 255.0,
        };
    }
    out
}

/// 两套已预转换主色的对称加权距离，0 表示一致。
///
/// 每个色项匹配对方 OKLab 空间里最近的一项，按自身权重求平均，再对两个方向取均值。
pub fn prepared_distance(left: &[LabColor], right: &[LabColor]) -> f32 {
    ((nearest_prepared(left, right) + nearest_prepared(right, left)) * 0.5).min(1.0)
}

fn nearest_prepared(source: &[LabColor], target: &[LabColor]) -> f32 {
    let total: f32 = source.iter().map(|entry| entry.weight).sum();
    if total <= f32::EPSILON {
        return 0.0;
    }
    let mut sum = 0.0_f32;
    for entry in source {
        let mut nearest = f32::INFINITY;
        for other in target {
            let dl = entry.l - other.l;
            let da = entry.a - other.a;
            let db = entry.b - other.b;
            nearest = nearest.min((dl * dl + da * da + db * db).sqrt());
        }
        if nearest.is_finite() {
            sum += entry.weight * nearest;
        }
    }
    sum / total
}

fn distance_squared(left: &[f32; 3], right: &[f32; 3]) -> f64 {
    left.iter()
        .zip(right.iter())
        .map(|(a, b)| {
            let delta = f64::from(*a) - f64::from(*b);
            delta * delta
        })
        .sum()
}

fn oklab_from_srgb(red: u8, green: u8, blue: u8) -> [f32; 3] {
    let r = srgb_to_linear(f32::from(red) / 255.0);
    let g = srgb_to_linear(f32::from(green) / 255.0);
    let b = srgb_to_linear(f32::from(blue) / 255.0);

    let l = 0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_99 * b;
    let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
    let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;

    let l = l.cbrt();
    let m = m.cbrt();
    let s = s.cbrt();

    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

fn srgb_from_oklab(lab: [f32; 3]) -> [u8; 3] {
    let l = lab[0] + 0.396_337_78 * lab[1] + 0.215_803_76 * lab[2];
    let m = lab[0] - 0.105_561_346 * lab[1] - 0.063_854_17 * lab[2];
    let s = lab[0] - 0.089_484_18 * lab[1] - 1.291_485_5 * lab[2];

    let l = l * l * l;
    let m = m * m * m;
    let s = s * s * s;

    let red = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s;
    let green = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s;
    let blue = -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s;

    [
        linear_to_srgb(red),
        linear_to_srgb(green),
        linear_to_srgb(blue),
    ]
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f32) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, Rgb, RgbImage};

    use super::*;

    fn two_color() -> DynamicImage {
        let mut image = RgbImage::new(128, 64);
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            *pixel = if x < 64 {
                Rgb([210, 40, 30])
            } else {
                Rgb([30, 60, 200])
            };
        }
        DynamicImage::ImageRgb8(image)
    }

    fn solid(color: [u8; 3]) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(80, 80, Rgb(color)))
    }

    fn entries(colors: &[u8]) -> Vec<([u8; 3], u8)> {
        colors
            .chunks_exact(4)
            .map(|slot| ([slot[0], slot[1], slot[2]], slot[3]))
            .collect()
    }

    #[test]
    fn returns_requested_palette_sizes() {
        let (colors4, colors8) = dominant(&two_color());
        assert_eq!(colors4.len(), COLORS4_BYTES);
        assert_eq!(colors8.len(), COLORS8_BYTES);
        assert_eq!(colors4.chunks_exact(4).len(), 4);
        assert_eq!(colors8.chunks_exact(4).len(), 8);
    }

    #[test]
    fn weights_sum_to_255() {
        let (colors4, colors8) = dominant(&two_color());
        assert_eq!(
            colors4
                .chunks_exact(4)
                .map(|slot| slot[3] as u32)
                .sum::<u32>(),
            255
        );
        assert_eq!(
            colors8
                .chunks_exact(4)
                .map(|slot| slot[3] as u32)
                .sum::<u32>(),
            255
        );
    }

    #[test]
    fn finds_both_colors() {
        let (colors4, _) = dominant(&two_color());
        let palette = entries(&colors4);
        let red = palette.iter().find(|(rgb, _)| rgb[0] > 150 && rgb[2] < 100);
        let blue = palette.iter().find(|(rgb, _)| rgb[2] > 150 && rgb[0] < 100);
        assert!(red.is_some(), "palette: {palette:?}");
        assert!(blue.is_some(), "palette: {palette:?}");
        assert!(red.unwrap().1 >= 60);
        assert!(blue.unwrap().1 >= 60);
    }

    #[test]
    fn solid_image_has_single_dominant_entry() {
        let (colors4, _) = dominant(&solid([90, 120, 150]));
        let palette = entries(&colors4);
        assert!(palette[0].0[0] > 80 && palette[0].0[0] < 100);
        assert!(palette[0].1 > 200);
    }

    #[test]
    fn is_deterministic() {
        let first = dominant(&two_color());
        let second = dominant(&two_color());
        assert_eq!(first, second);
    }

    /// 测试里直接走线上实现（`prepare_palette` + `prepared_distance`），避免出现第二套距离算法。
    fn distance4(left: &[u8; COLORS4_BYTES], right: &[u8; COLORS4_BYTES]) -> f32 {
        prepared_distance(&prepare_palette::<4>(left), &prepare_palette::<4>(right))
    }

    #[test]
    fn palette_distance_is_zero_for_identical_palettes() {
        let (colors4, colors8) = dominant(&two_color());
        assert_eq!(distance4(&colors4, &colors4), 0.0);
        let prepared = prepare_palette::<8>(&colors8);
        assert_eq!(prepared_distance(&prepared, &prepared), 0.0);
    }

    #[test]
    fn palette_distance_grows_with_color_difference() {
        let (red, _) = dominant(&solid([200, 30, 30]));
        let (near_red, _) = dominant(&solid([190, 40, 40]));
        let (blue, _) = dominant(&solid([30, 40, 200]));
        let near = distance4(&red, &near_red);
        let far = distance4(&red, &blue);
        assert!(near < far, "near={near} far={far}");
        assert!(far > 0.1);
        assert_eq!(near, distance4(&near_red, &red));
    }
}

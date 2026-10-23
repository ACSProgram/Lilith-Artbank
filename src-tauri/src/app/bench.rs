use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use image::{DynamicImage, Rgb, RgbImage};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    app::error::AppResult,
    imageops::thumbnail,
    library::thumbs::{ThumbLocation, ThumbPack},
};

pub const BENCH_COUNT: usize = 150;
/// 默认读取轮数；与 WebView2 侧的多轮加载保持同一量级，便于对照。
pub const READ_ROUNDS: usize = 20;
const BENCH_SIZE: u32 = 512;

pub struct ThumbBench {
    directory: PathBuf,
    pack: Arc<ThumbPack>,
    entries: Vec<ThumbLocation>,
    thumb_bytes: u64,
    build_ms: f64,
}

impl ThumbBench {
    pub fn location(&self, index: usize) -> Option<ThumbLocation> {
        self.entries.get(index).copied()
    }

    pub fn pack(&self) -> &ThumbPack {
        &self.pack
    }

    pub fn measure_read(&self, rounds: usize) -> AppResult<ThumbReadStats> {
        let rounds = rounds.max(1);
        let mut samples = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let start = Instant::now();
            for location in &self.entries {
                let _ = self.pack.read(*location)?;
            }
            samples.push(start.elapsed());
        }
        Ok(ThumbReadStats {
            p50_ms: ms(percentile(&samples, 0.5)),
            p95_ms: ms(percentile(&samples, 0.95)),
            total_bytes: self.thumb_bytes,
        })
    }
}

impl Drop for ThumbBench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThumbBenchInfo {
    pub count: usize,
    pub thumb_bytes: u64,
    pub build_ms: f64,
    pub read_p50_ms: f64,
    pub read_p95_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThumbReadStats {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub total_bytes: u64,
}

pub fn prepare() -> AppResult<(Arc<ThumbBench>, ThumbBenchInfo)> {
    let directory = std::env::temp_dir().join(format!("lilith-artbank-bench-{}", Uuid::new_v4()));
    match build(&directory) {
        Ok(bench) => {
            let read = bench.measure_read(READ_ROUNDS)?;
            let info = ThumbBenchInfo {
                count: BENCH_COUNT,
                thumb_bytes: bench.thumb_bytes,
                build_ms: bench.build_ms,
                read_p50_ms: read.p50_ms,
                read_p95_ms: read.p95_ms,
            };
            Ok((Arc::new(bench), info))
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&directory);
            Err(error)
        }
    }
}

fn build(directory: &std::path::Path) -> AppResult<ThumbBench> {
    std::fs::create_dir_all(directory)?;
    let pack = Arc::new(ThumbPack::open(directory)?);

    let start = Instant::now();
    let mut entries = Vec::with_capacity(BENCH_COUNT);
    let mut thumb_bytes = 0_u64;
    for index in 0..BENCH_COUNT {
        let image = bench_image(index as u32);
        let thumbnail = thumbnail::encode(&image)?;
        thumb_bytes += thumbnail.bytes.len() as u64;
        entries.push(pack.append(&thumbnail.bytes)?);
    }

    Ok(ThumbBench {
        directory: directory.to_path_buf(),
        pack,
        entries,
        thumb_bytes,
        build_ms: ms(start.elapsed()),
    })
}

fn bench_image(seed: u32) -> DynamicImage {
    const SHAPE_COUNT: usize = 5;
    let mut random = Rng::new(seed.wrapping_mul(0x9e37_79b9).wrapping_add(1));

    let top = [random.byte(), random.byte(), random.byte()];
    let bottom = [random.byte(), random.byte(), random.byte()];
    let mut shapes = [(0_i32, 0_i32, 0_i32, [0_u8; 3]); SHAPE_COUNT];
    for shape in &mut shapes {
        shape.0 = (random.next() % BENCH_SIZE) as i32;
        shape.1 = (random.next() % BENCH_SIZE) as i32;
        shape.2 = 90 + (random.next() % 150) as i32;
        shape.3 = [random.byte(), random.byte(), random.byte()];
    }

    let mut image = RgbImage::new(BENCH_SIZE, BENCH_SIZE);
    let last = (BENCH_SIZE - 1) as f32;
    for y in 0..BENCH_SIZE {
        let t = y as f32 / last;
        for x in 0..BENCH_SIZE {
            let mut rgb = [
                lerp(top[0], bottom[0], t),
                lerp(top[1], bottom[1], t),
                lerp(top[2], bottom[2], t),
            ];
            for (center_x, center_y, radius, color) in &shapes {
                let dx = x as i32 - center_x;
                let dy = y as i32 - center_y;
                if dx * dx + dy * dy < radius * radius {
                    rgb[0] = blend(rgb[0], color[0], 4);
                    rgb[1] = blend(rgb[1], color[1], 4);
                    rgb[2] = blend(rgb[2], color[2], 4);
                }
            }
            let stripe = if ((x / 24) + (y / 31)) & 1 == 0 {
                10
            } else {
                -10
            };
            let noise = (random.next() & 31) as i32 - 16;
            image.put_pixel(
                x,
                y,
                Rgb([
                    clamp(rgb[0] as i32 + stripe + noise),
                    clamp(rgb[1] as i32 + stripe + noise),
                    clamp(rgb[2] as i32 + stripe + noise),
                ]),
            );
        }
    }
    DynamicImage::ImageRgb8(image)
}

struct Rng(u32);

impl Rng {
    fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9e37_79b9 } else { seed })
    }

    fn next(&mut self) -> u32 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 17;
        value ^= value << 5;
        self.0 = value;
        value
    }

    fn byte(&mut self) -> u8 {
        (self.next() & 0xff) as u8
    }
}

fn lerp(from: u8, to: u8, t: f32) -> u8 {
    (from as f32 + (to as f32 - from as f32) * t).round() as u8
}

fn blend(from: u8, to: u8, shift: u32) -> u8 {
    ((from as u32 * ((1 << shift) - 1) + to as u32) >> shift) as u8
}

fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn percentile(samples: &[Duration], percentile: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() as f64 - 1.0) * percentile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "生成 150 张基准样本；用 cargo test -- --ignored --nocapture perf_thumb_bench_build 运行"]
    fn perf_thumb_bench_build() {
        let wall = Instant::now();
        let (bench, info) = prepare().expect("prepare bench");
        println!(
            "[perf] thumb bench: {} thumbs, {:.2} MB, build={:.0}ms, read p50={:.2}ms p95={:.2}ms, wall={:.0}ms",
            info.count,
            info.thumb_bytes as f64 / 1048576.0,
            info.build_ms,
            info.read_p50_ms,
            info.read_p95_ms,
            wall.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(info.count, BENCH_COUNT);
        assert!(info.read_p50_ms > 0.0);
        assert!(bench.location(BENCH_COUNT - 1).is_some());
    }
}

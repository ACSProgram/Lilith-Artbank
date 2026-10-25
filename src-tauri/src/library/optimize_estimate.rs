//! 图片空间优化的只读估算：分层抽样、真实编码、外推与分文件夹明细。
//!
//! 估算不写数据库、不碰原文件、不产生临时文件。样本按“格式组 × bpp 桶”分层，
//! 按候选体积分配样本量，避免随机抽样系统性低估高 bpp 的图。

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use image::{ColorType, DynamicImage};
use rayon::prelude::*;
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    imageops::optimize::{
        self, LadderOutcome, OptimizeFormat, OptimizeOptions, SkipReason, SourceInfo,
    },
    import::decode,
    library::{
        BatchReporter, db::LibraryDb, metadata, paths::LibraryPaths, placeholders, service::now_ms,
    },
};

const BUCKET_BOUNDS: [f64; 5] = [0.10, 0.15, 0.22, 0.32, 0.50];
const BUCKET_COUNT: usize = 6;
/// 有损源 / 无损源两组 × 6 个 bpp 桶。
const STRATUM_COUNT: usize = BUCKET_COUNT * 2;
const MIN_PER_STRATUM: usize = 3;
const BOOTSTRAP_ROUNDS: usize = 300;
const MAX_SAMPLE_FAILURES: usize = 20;
const MAX_THREADS: usize = 8;
const CHUNK: usize = 400;

pub const SAMPLE_CHOICES: [usize; 3] = [100, 200, 500];

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkipBucket {
    pub count: u64,
    pub bytes: u64,
}

impl SkipBucket {
    fn add(&mut self, count: u64, bytes: u64) {
        self.count += count;
        self.bytes += bytes;
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityCount {
    pub quality: u8,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurvePoint {
    /// 已处理的候选体积占比。
    pub coverage: f64,
    pub savings_bytes: u64,
    /// 按像素折算的累计耗时；与执行顺序一致，最后一点等于总预计耗时。
    pub elapsed_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderEstimate {
    pub folder_id: i64,
    pub path: String,
    pub count: u64,
    pub bytes: u64,
    pub savings_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeEstimate {
    pub task_id: String,
    pub created_at: i64,
    pub options_fingerprint: String,
    /// 实际参与编码的样本张数与原始字节数；含失败样本。
    pub sample_count: u64,
    pub sample_bytes: u64,
    pub coverage: f64,
    /// 样本中编码成功图片的平均局部坏点占比；只作诊断，不参与裁决。
    pub sample_bad_pixel_ratio: f64,
    pub total_count: u64,
    pub total_bytes: u64,
    pub candidate_count: u64,
    pub candidate_bytes: u64,
    pub process_count: u64,
    pub process_bytes: u64,
    pub skip_compact: SkipBucket,
    pub skip_optimized: SkipBucket,
    pub skip_transparent: SkipBucket,
    pub skip_depth: SkipBucket,
    pub skip_quality: SkipBucket,
    pub skip_not_smaller: SkipBucket,
    pub failed_count: u64,
    /// 失败样本按比例外推出的体积；与上面的跳过分类一起构成“跳过”总量。
    pub failed_bytes: u64,
    pub savings_bytes: u64,
    pub savings_margin_bytes: u64,
    pub predicted_bytes: u64,
    pub estimated_seconds: u64,
    pub quality_counts: Vec<QualityCount>,
    /// 按 bpp 从高到低执行时的累计节省；x 轴是候选体积占比。
    pub curve: Vec<CurvePoint>,
    pub folders: Vec<FolderEstimate>,
    pub sample_failures: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct StratumTotals {
    count: u64,
    bytes: u64,
    pixels: u64,
}

#[derive(Debug, Clone, Default)]
struct FolderTotals {
    path: String,
    count: u64,
    bytes: u64,
    stratum_bytes: [u64; STRATUM_COUNT],
}

#[derive(Debug, Clone)]
struct CandidateRow {
    id: i64,
    content_hash: String,
    file_ext: String,
    folder_id: i64,
    folder_path: String,
    file_size: i64,
    width: u32,
    height: u32,
    format: String,
    has_alpha: bool,
}

impl CandidateRow {
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    fn lossless_source(&self) -> bool {
        !matches!(self.format.as_str(), "jpeg" | "jpg" | "webp")
    }
}

#[derive(Debug, Clone)]
struct SampleOutcome {
    old_bytes: u64,
    new_bytes: u64,
    quality: Option<u8>,
    skip: Option<SkipReason>,
    failed: bool,
    source_pixels: u64,
    read_secs: f64,
    bad_pixel_ratio: f64,
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        (self.next() % bound as u64) as usize
    }
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ u64::from(std::process::id()).wrapping_mul(0x9E37_79B9)
}

fn bucket_for(bpp: f64) -> usize {
    BUCKET_BOUNDS
        .iter()
        .position(|bound| bpp < *bound)
        .unwrap_or(BUCKET_BOUNDS.len())
}

fn stratum_index(lossless: bool, bucket: usize) -> usize {
    usize::from(lossless) * BUCKET_COUNT + bucket.min(BUCKET_COUNT - 1)
}

fn bit_depth(image: &DynamicImage) -> u8 {
    match image.color() {
        ColorType::L16 | ColorType::La16 | ColorType::Rgb16 | ColorType::Rgba16 => 16,
        _ => 8,
    }
}

fn load_rows(database: &LibraryDb, ids: &[i64]) -> AppResult<Vec<CandidateRow>> {
    let mut rows = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let sql = format!(
            "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                    folders.path, images.file_size, images.width, images.height,
                    images.format, images.has_alpha
             FROM images JOIN folders ON folders.id = images.folder_id
             WHERE images.deleted_at IS NULL
               AND images.id IN ({})",
            placeholders(chunk.len())
        );
        let mut part = database.with_reader(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok(CandidateRow {
                        id: row.get(0)?,
                        content_hash: row.get(1)?,
                        file_ext: row.get(2)?,
                        folder_id: row.get(3)?,
                        folder_path: row.get(4)?,
                        file_size: row.get(5)?,
                        width: row.get(6)?,
                        height: row.get(7)?,
                        format: row.get(8)?,
                        has_alpha: row.get::<_, i64>(9)? != 0,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.append(&mut part);
    }
    Ok(rows)
}

/// 估算入口：只读；返回 `None` 表示中途取消。
#[allow(clippy::too_many_arguments)]
pub fn estimate(
    database: &LibraryDb,
    root: &Path,
    task_id: &str,
    image_ids: &[i64],
    options: &OptimizeOptions,
    sample_budget: usize,
    options_fingerprint: &str,
    reporter: &dyn BatchReporter,
) -> AppResult<Option<OptimizeEstimate>> {
    options.validate()?;
    let sample_budget = sample_budget.clamp(1, 2000);
    let mut totals = std::array::from_fn::<_, STRATUM_COUNT, _>(|_| StratumTotals::default());
    let mut strata_ids: [Vec<i64>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    let mut folders: HashMap<i64, FolderTotals> = HashMap::new();
    let mut total_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut skip_compact = SkipBucket::default();
    let mut skip_transparent = SkipBucket::default();

    reporter.message("正在统计候选图片…");
    reporter.report(0, 0);
    let mut scanned = 0_usize;
    for chunk in image_ids.chunks(CHUNK) {
        if reporter.cancelled() {
            return Ok(None);
        }
        let rows = load_rows(database, chunk)?;
        scanned += chunk.len();
        for row in rows {
            total_count += 1;
            let bytes = row.file_size.max(0) as u64;
            total_bytes += bytes;
            let bpp = bytes as f64 * 8.0 / row.pixels().max(1) as f64;
            if bpp < optimize::BPP_THRESHOLD {
                skip_compact.add(1, bytes);
                continue;
            }
            if row.has_alpha && options.output_format == OptimizeFormat::Jpeg {
                skip_transparent.add(1, bytes);
                continue;
            }
            let stratum = stratum_index(row.lossless_source(), bucket_for(bpp));
            totals[stratum].count += 1;
            totals[stratum].bytes += bytes;
            totals[stratum].pixels += row.pixels();
            let folder = folders
                .entry(row.folder_id)
                .or_insert_with(|| FolderTotals {
                    path: row.folder_path.clone(),
                    ..FolderTotals::default()
                });
            folder.count += 1;
            folder.bytes += bytes;
            folder.stratum_bytes[stratum] += bytes;
            strata_ids[stratum].push(row.id);
        }
        reporter.message(&format!(
            "正在统计候选图片… {}/{}",
            scanned.min(image_ids.len()),
            image_ids.len()
        ));
    }

    // schema v5 还没有 optimized_at；批次 2 接入标记列后，这里改为读取该列。
    let skip_optimized = SkipBucket::default();

    let candidate_count: u64 = totals.iter().map(|stratum| stratum.count).sum();
    let candidate_bytes: u64 = totals.iter().map(|stratum| stratum.bytes).sum();
    let candidate_pixels: u64 = totals.iter().map(|stratum| stratum.pixels).sum();
    if candidate_count == 0 {
        return Ok(Some(empty_estimate(
            task_id,
            options_fingerprint,
            total_count,
            total_bytes,
            skip_compact,
            skip_optimized,
            skip_transparent,
        )));
    }

    let allocation = allocate_samples(&totals, sample_budget);
    let mut rng = Rng::new(seed());
    let mut sample_tasks: Vec<(usize, i64, u64)> = Vec::new();
    for (stratum, &take) in allocation.iter().enumerate() {
        if take == 0 || strata_ids[stratum].is_empty() {
            continue;
        }
        // 行已被删除时用该层平均像素兜底，只影响失败样本的耗时外推。
        let pixels = totals[stratum].pixels / totals[stratum].count.max(1);
        for index in choose_indices(strata_ids[stratum].len(), take, &mut rng) {
            let id = strata_ids[stratum][index];
            sample_tasks.push((stratum, id, pixels));
        }
    }
    let sample_total = sample_tasks.len();
    if sample_total == 0 {
        return Ok(Some(empty_estimate(
            task_id,
            options_fingerprint,
            total_count,
            total_bytes,
            skip_compact,
            skip_optimized,
            skip_transparent,
        )));
    }

    let sample_ids: Vec<i64> = sample_tasks.iter().map(|(_, id, _)| *id).collect();
    let sample_rows: HashMap<i64, CandidateRow> = load_rows(database, &sample_ids)?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();

    let threads = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, MAX_THREADS);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|error| AppError::message(format!("无法创建估算线程池：{error}")))?;

    reporter.message(&format!("正在编码样本 0 / {sample_total} 张"));
    reporter.report(0, sample_total);
    let done = AtomicUsize::new(0);
    let started = Instant::now();
    let records = pool.install(|| {
        sample_tasks
            .par_iter()
            .filter_map(|(stratum, id, db_pixels)| {
                if reporter.cancelled() {
                    return None;
                }
                let row = sample_rows.get(id);
                let (outcome, error) = match row {
                    Some(row) => match sample_one(root, row, options) {
                        Ok(outcome) => (outcome, None),
                        Err(error) => (
                            failed_outcome(*db_pixels, row.file_size.max(0) as u64),
                            Some(error.to_string()),
                        ),
                    },
                    None => (
                        failed_outcome(*db_pixels, 0),
                        Some("图片行已被删除".to_owned()),
                    ),
                };
                let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                reporter.report(completed, sample_total);
                reporter.message(&format!("正在编码样本 {completed} / {sample_total} 张"));
                Some((*stratum, *id, outcome, error))
            })
            .collect::<Vec<_>>()
    });
    let elapsed = started.elapsed().as_secs_f64();
    if reporter.cancelled() {
        return Ok(None);
    }

    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    let mut sample_bytes = 0_u64;
    let mut sample_pixels = 0_u64;
    let mut read_secs = 0.0_f64;
    let mut sample_failures = Vec::new();
    let mut bad_pixel_sum = 0.0_f64;
    let mut bad_pixel_count = 0_u64;
    for (stratum, id, outcome, error) in records {
        sample_bytes += outcome.old_bytes;
        sample_pixels += outcome.source_pixels;
        read_secs += outcome.read_secs;
        if outcome.quality.is_some() {
            bad_pixel_sum += outcome.bad_pixel_ratio;
            bad_pixel_count += 1;
        }
        if let Some(error) = error
            && sample_failures.len() < MAX_SAMPLE_FAILURES
        {
            sample_failures.push(format!("图片 #{id}：{error}"));
        }
        samples[stratum].push(outcome);
    }

    let savings_bytes = point_savings(&totals, &samples);
    let savings_margin_bytes = bootstrap_margin(&totals, &samples, BOOTSTRAP_ROUNDS);
    let (skip_depth, skip_quality, skip_not_smaller, failed_count, failed_bytes) =
        cumulative_skips(&totals, &samples);
    let skipped_count =
        skip_depth.count + skip_quality.count + skip_not_smaller.count + failed_count;
    let skipped_bytes =
        skip_depth.bytes + skip_quality.bytes + skip_not_smaller.bytes + failed_bytes;
    let process_count = candidate_count.saturating_sub(skipped_count);
    let process_bytes = candidate_bytes.saturating_sub(skipped_bytes);
    let predicted_bytes = total_bytes.saturating_sub(savings_bytes);

    let mut quality_counts: HashMap<u8, u64> = HashMap::new();
    for stratum in &samples {
        for sample in stratum {
            if let Some(quality) = sample.quality {
                *quality_counts.entry(quality).or_default() += 1;
            }
        }
    }
    let mut quality_counts: Vec<QualityCount> = quality_counts
        .into_iter()
        .map(|(quality, count)| QualityCount { quality, count })
        .collect();
    quality_counts.sort_by_key(|entry| entry.quality);

    let rates = stratum_rates(&totals, &samples);
    let mut folder_estimates: Vec<FolderEstimate> = folders
        .into_iter()
        .map(|(folder_id, folder)| {
            let savings: u64 = folder
                .stratum_bytes
                .iter()
                .zip(rates.iter())
                .map(|(bytes, rate)| (*bytes as f64 * rate).round() as u64)
                .sum();
            FolderEstimate {
                folder_id,
                path: folder.path,
                count: folder.count,
                bytes: folder.bytes,
                savings_bytes: savings.min(folder.bytes),
            }
        })
        .collect();
    folder_estimates.sort_by(|left, right| {
        right
            .savings_bytes
            .cmp(&left.savings_bytes)
            .then_with(|| right.bytes.cmp(&left.bytes))
    });

    let estimated_seconds = estimate_seconds(
        elapsed,
        read_secs,
        threads,
        sample_bytes,
        candidate_pixels,
        candidate_bytes,
        predicted_bytes,
        sample_pixels,
    );

    Ok(Some(OptimizeEstimate {
        task_id: task_id.to_owned(),
        created_at: now_ms(),
        options_fingerprint: options_fingerprint.to_owned(),
        sample_count: sample_total as u64,
        sample_bytes,
        coverage: sample_bytes as f64 / candidate_bytes.max(1) as f64,
        sample_bad_pixel_ratio: if bad_pixel_count > 0 {
            bad_pixel_sum / bad_pixel_count as f64
        } else {
            0.0
        },
        total_count,
        total_bytes,
        candidate_count,
        candidate_bytes,
        process_count,
        process_bytes,
        skip_compact,
        skip_optimized,
        skip_transparent,
        skip_depth,
        skip_quality,
        skip_not_smaller,
        failed_count,
        failed_bytes,
        savings_bytes,
        savings_margin_bytes,
        predicted_bytes,
        estimated_seconds,
        quality_counts,
        curve: benefit_curve(&totals, &samples, estimated_seconds),
        folders: folder_estimates,
        sample_failures,
    }))
}

fn empty_estimate(
    task_id: &str,
    fingerprint: &str,
    total_count: u64,
    total_bytes: u64,
    skip_compact: SkipBucket,
    skip_optimized: SkipBucket,
    skip_transparent: SkipBucket,
) -> OptimizeEstimate {
    OptimizeEstimate {
        task_id: task_id.to_owned(),
        created_at: now_ms(),
        options_fingerprint: fingerprint.to_owned(),
        sample_count: 0,
        sample_bytes: 0,
        coverage: 0.0,
        sample_bad_pixel_ratio: 0.0,
        total_count,
        total_bytes,
        candidate_count: 0,
        candidate_bytes: 0,
        process_count: 0,
        process_bytes: 0,
        skip_compact,
        skip_optimized,
        skip_transparent,
        skip_depth: SkipBucket::default(),
        skip_quality: SkipBucket::default(),
        skip_not_smaller: SkipBucket::default(),
        failed_count: 0,
        failed_bytes: 0,
        savings_bytes: 0,
        savings_margin_bytes: 0,
        predicted_bytes: total_bytes,
        estimated_seconds: 0,
        quality_counts: Vec::new(),
        curve: Vec::new(),
        folders: Vec::new(),
        sample_failures: Vec::new(),
    }
}

fn failed_outcome(pixels: u64, bytes: u64) -> SampleOutcome {
    SampleOutcome {
        old_bytes: bytes,
        new_bytes: bytes,
        quality: None,
        skip: None,
        failed: true,
        source_pixels: pixels,
        read_secs: 0.0,
        bad_pixel_ratio: 0.0,
    }
}

fn sample_one(
    root: &Path,
    row: &CandidateRow,
    options: &OptimizeOptions,
) -> AppResult<SampleOutcome> {
    let paths = LibraryPaths::new(root);
    let path = metadata::content_path(&paths, &row.folder_path, &row.content_hash, &row.file_ext)?;
    let read_started = Instant::now();
    let source_bytes = std::fs::read(&path)?;
    let read_secs = read_started.elapsed().as_secs_f64();
    let decoded = decode::decode(&path)?;

    let source = SourceInfo {
        format: row.format.clone(),
        file_size: source_bytes.len() as i64,
        width: decoded.width,
        height: decoded.height,
        has_alpha: row.has_alpha || decoded.has_alpha,
        optimized_at: 0,
        bit_depth: bit_depth(&decoded.image),
    };
    let old_bytes = source_bytes.len() as u64;
    let source_pixels = u64::from(decoded.width) * u64::from(decoded.height);
    let outcome = |quality: Option<u8>, new_bytes: u64, skip: Option<SkipReason>| SampleOutcome {
        old_bytes,
        new_bytes,
        quality,
        skip,
        failed: false,
        source_pixels,
        read_secs,
        bad_pixel_ratio: 0.0,
    };

    let plan = match optimize::plan(&source, options) {
        Ok(plan) => plan,
        Err(reason) => return Ok(outcome(None, old_bytes, Some(reason))),
    };
    let prepared = optimize::prepare(decoded.image, options.max_long_edge);
    let source_jpeg = matches!(row.format.as_str(), "jpeg" | "jpg")
        .then_some(source_bytes.as_slice())
        .filter(|_| options.output_format == OptimizeFormat::Jpeg);
    match optimize::encode_ladder(&prepared, &plan, options, old_bytes, source_jpeg)? {
        LadderOutcome::Encoded(candidate) => {
            let mut sample = outcome(Some(candidate.quality), candidate.bytes.len() as u64, None);
            sample.bad_pixel_ratio = candidate.bad_pixel_ratio;
            Ok(sample)
        }
        LadderOutcome::Skipped(reason) => Ok(outcome(None, old_bytes, Some(reason))),
    }
}

/// 按候选体积分配样本；每个有候选的层至少 3 张，余量按字节份额分配。
fn allocate_samples(
    totals: &[StratumTotals; STRATUM_COUNT],
    budget: usize,
) -> [usize; STRATUM_COUNT] {
    let mut allocation = [0_usize; STRATUM_COUNT];
    let total_count: u64 = totals.iter().map(|stratum| stratum.count).sum();
    if total_count == 0 {
        return allocation;
    }
    if budget as u64 >= total_count {
        for (index, stratum) in totals.iter().enumerate() {
            allocation[index] = stratum.count as usize;
        }
        return allocation;
    }

    let mut remaining = budget;
    for (index, stratum) in totals.iter().enumerate() {
        if stratum.count == 0 {
            continue;
        }
        let base = MIN_PER_STRATUM.min(stratum.count as usize).min(remaining);
        allocation[index] = base;
        remaining -= base;
    }
    let mut open: Vec<usize> = (0..STRATUM_COUNT)
        .filter(|index| (allocation[*index] as u64) < totals[*index].count)
        .collect();
    while remaining > 0 && !open.is_empty() {
        let total_bytes: u64 = open.iter().map(|index| totals[*index].bytes).sum();
        let mut added = 0_usize;
        for index in &open {
            let share = if total_bytes == 0 {
                0
            } else {
                (remaining as u128 * totals[*index].bytes as u128 / total_bytes as u128) as usize
            };
            let capacity = totals[*index].count as usize - allocation[*index];
            let take = share.min(capacity);
            allocation[*index] += take;
            added += take;
        }
        remaining -= added;
        open.retain(|index| (allocation[*index] as u64) < totals[*index].count);
        if added == 0 {
            let mut progressed = false;
            while remaining > 0 {
                for index in &open {
                    if remaining == 0 {
                        break;
                    }
                    if (allocation[*index] as u64) < totals[*index].count {
                        allocation[*index] += 1;
                        remaining -= 1;
                        progressed = true;
                    }
                }
                if !progressed {
                    break;
                }
            }
            break;
        }
    }
    allocation
}

fn choose_indices(length: usize, take: usize, rng: &mut Rng) -> Vec<usize> {
    let take = take.min(length);
    if take == 0 {
        return Vec::new();
    }
    if take == length {
        return (0..length).collect();
    }
    if take * 3 >= length {
        let mut indices: Vec<usize> = (0..length).collect();
        for position in 0..take {
            let swap = position + rng.below(length - position);
            indices.swap(position, swap);
        }
        indices.truncate(take);
        return indices;
    }
    let mut chosen = HashSet::with_capacity(take);
    while chosen.len() < take {
        chosen.insert(rng.below(length));
    }
    chosen.into_iter().collect()
}

fn stratum_rates(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
) -> [f64; STRATUM_COUNT] {
    let mut rates = [0.0_f64; STRATUM_COUNT];
    for (index, outcomes) in samples.iter().enumerate() {
        if totals[index].count == 0 || outcomes.is_empty() {
            continue;
        }
        let old: u64 = outcomes.iter().map(|sample| sample.old_bytes).sum();
        let new: u64 = outcomes.iter().map(|sample| sample.new_bytes).sum();
        if old > 0 {
            rates[index] = (1.0 - new as f64 / old as f64).clamp(0.0, optimize::MAX_SIZE_RATIO);
        }
    }
    rates
}

fn point_savings(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
) -> u64 {
    let rates = stratum_rates(totals, samples);
    totals
        .iter()
        .zip(rates.iter())
        .map(|(stratum, rate)| (stratum.bytes as f64 * rate).round() as u64)
        .sum()
}

fn benefit_curve(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
    total_seconds: u64,
) -> Vec<CurvePoint> {
    let rates = stratum_rates(totals, samples);
    let candidate_bytes: u64 = totals.iter().map(|stratum| stratum.bytes).sum();
    if candidate_bytes == 0 {
        return Vec::new();
    }
    let candidate_pixels: u64 = totals.iter().map(|stratum| stratum.pixels).sum();
    let mut order: Vec<usize> = totals
        .iter()
        .enumerate()
        .filter(|(_, stratum)| stratum.count > 0)
        .map(|(index, _)| index)
        .collect();
    // bpp 桶从高到低；同一桶内先有损源、后无损源。
    order.sort_by_key(|index| {
        (
            std::cmp::Reverse(index % BUCKET_COUNT),
            usize::from(*index >= BUCKET_COUNT),
        )
    });

    let mut points = vec![CurvePoint {
        coverage: 0.0,
        savings_bytes: 0,
        elapsed_seconds: 0,
    }];
    let mut processed = 0_u64;
    let mut pixels = 0_u64;
    let mut savings = 0_u64;
    for index in order {
        processed += totals[index].bytes;
        pixels += totals[index].pixels;
        savings += (totals[index].bytes as f64 * rates[index]).round() as u64;
        points.push(CurvePoint {
            coverage: processed as f64 / candidate_bytes as f64,
            savings_bytes: savings,
            // 耗时按像素走：解码与编码的开销跟着像素数，不跟字节数。
            elapsed_seconds: if candidate_pixels == 0 {
                0
            } else {
                (total_seconds as f64 * pixels as f64 / candidate_pixels as f64).round() as u64
            },
        });
    }
    points
}

fn cumulative_skips(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
) -> (SkipBucket, SkipBucket, SkipBucket, u64, u64) {
    let mut depth = SkipBucket::default();
    let mut quality = SkipBucket::default();
    let mut not_smaller = SkipBucket::default();
    let mut failed = 0_u64;
    let mut failed_bytes = 0_u64;
    for (index, outcomes) in samples.iter().enumerate() {
        let total = &totals[index];
        if total.count == 0 || outcomes.is_empty() {
            continue;
        }
        let sample_count = outcomes.len() as u64;
        let mut depth_samples = 0_u64;
        let mut quality_samples = 0_u64;
        let mut not_smaller_samples = 0_u64;
        let mut failed_samples = 0_u64;
        for sample in outcomes {
            if sample.failed {
                failed_samples += 1;
                continue;
            }
            match sample.skip {
                Some(SkipReason::SixteenBit) => depth_samples += 1,
                Some(SkipReason::QualityNotMet) => quality_samples += 1,
                Some(SkipReason::NotSmallEnough) => not_smaller_samples += 1,
                _ => {}
            }
        }
        depth.add(
            scale(total.count, depth_samples, sample_count),
            scale(total.bytes, depth_samples, sample_count),
        );
        quality.add(
            scale(total.count, quality_samples, sample_count),
            scale(total.bytes, quality_samples, sample_count),
        );
        not_smaller.add(
            scale(total.count, not_smaller_samples, sample_count),
            scale(total.bytes, not_smaller_samples, sample_count),
        );
        failed += scale(total.count, failed_samples, sample_count);
        failed_bytes += scale(total.bytes, failed_samples, sample_count);
    }
    (depth, quality, not_smaller, failed, failed_bytes)
}

fn scale(total: u64, part: u64, sample_count: u64) -> u64 {
    if sample_count == 0 || part == 0 {
        return 0;
    }
    ((total as f64 * part as f64 / sample_count as f64).round() as u64).min(total)
}

fn bootstrap_margin(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
    rounds: usize,
) -> u64 {
    if rounds == 0 {
        return 0;
    }
    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    let mut savings = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let mut total = 0.0_f64;
        for (index, outcomes) in samples.iter().enumerate() {
            let stratum = &totals[index];
            if stratum.count == 0 || outcomes.is_empty() {
                continue;
            }
            let sample_count = outcomes.len();
            let mut old = 0_u64;
            let mut new = 0_u64;
            for _ in 0..sample_count {
                let sample = &outcomes[rng.below(sample_count)];
                old += sample.old_bytes;
                new += sample.new_bytes;
            }
            if old > 0 {
                let rate = 1.0 - new as f64 / old as f64;
                total += stratum.bytes as f64 * rate.clamp(0.0, optimize::MAX_SIZE_RATIO);
            }
        }
        savings.push(total);
    }
    let mean = savings.iter().sum::<f64>() / rounds as f64;
    let variance = savings
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f64>()
        / rounds as f64;
    (1.96 * variance.sqrt()).round() as u64
}

#[allow(clippy::too_many_arguments)]
fn estimate_seconds(
    elapsed: f64,
    read_secs: f64,
    threads: usize,
    sample_bytes: u64,
    candidate_pixels: u64,
    candidate_bytes: u64,
    predicted_bytes: u64,
    sample_pixels: u64,
) -> u64 {
    if candidate_pixels == 0 || sample_pixels == 0 {
        return 0;
    }
    let cpu = elapsed * candidate_pixels as f64 / sample_pixels as f64;
    let io = if read_secs > 0.001 && sample_bytes > 0 && candidate_bytes > 0 {
        // 采样读取时间按线程数折算成聚合带宽，与执行阶段使用同一档线程数。
        let bandwidth = sample_bytes as f64 * threads as f64 / read_secs;
        (candidate_bytes + predicted_bytes) as f64 / bandwidth
    } else {
        0.0
    };
    (cpu.max(io) * 1.15).round() as u64
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor};

    use image::{Rgb, RgbImage};
    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::{BatchReporter, service};

    #[derive(Default)]
    struct Recorder {
        completed: std::sync::atomic::AtomicUsize,
        messages: std::sync::Mutex<Vec<String>>,
    }

    impl BatchReporter for Recorder {
        fn report(&self, completed: usize, _total: usize) {
            self.completed
                .store(completed, std::sync::atomic::Ordering::Relaxed);
        }

        fn cancelled(&self) -> bool {
            false
        }

        fn message(&self, message: &str) {
            self.messages.lock().unwrap().push(message.to_owned());
        }
    }

    fn totals(count: u64, bytes: u64) -> StratumTotals {
        StratumTotals {
            count,
            bytes,
            pixels: count * 1_000_000,
        }
    }

    #[test]
    fn allocates_by_volume_with_a_floor_per_stratum() {
        let mut strata: [StratumTotals; STRATUM_COUNT] =
            std::array::from_fn(|_| Default::default());
        strata[0] = totals(1000, 1_000_000);
        strata[1] = totals(1000, 100_000);
        strata[2] = totals(1000, 10_000);
        let allocation = allocate_samples(&strata, 30);
        assert_eq!(allocation.iter().sum::<usize>(), 30);
        assert!(allocation[0] >= MIN_PER_STRATUM);
        assert!(allocation[1] >= MIN_PER_STRATUM);
        assert!(allocation[2] >= MIN_PER_STRATUM);
        assert!(allocation[0] > allocation[1]);
        assert!(allocation[1] > allocation[2]);
    }

    #[test]
    fn allocation_never_exceeds_stratum_size() {
        let mut strata: [StratumTotals; STRATUM_COUNT] =
            std::array::from_fn(|_| Default::default());
        strata[0] = totals(2, 1_000_000);
        strata[1] = totals(1000, 1_000);
        let allocation = allocate_samples(&strata, 100);
        assert_eq!(allocation[0], 2);
        assert!(allocation[1] <= 1000);
        assert_eq!(allocation.iter().sum::<usize>(), 100);
    }

    #[test]
    fn extrapolates_savings_and_skips_from_samples() {
        let mut strata: [StratumTotals; STRATUM_COUNT] =
            std::array::from_fn(|_| Default::default());
        strata[0] = totals(100, 10_000);
        let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
        let outcome =
            |new_bytes: u64, quality: Option<u8>, skip: Option<SkipReason>, failed: bool| {
                SampleOutcome {
                    old_bytes: 100,
                    new_bytes,
                    quality,
                    skip,
                    failed,
                    source_pixels: 1_000,
                    read_secs: 0.0,
                    bad_pixel_ratio: 0.0,
                }
            };
        samples[0] = vec![
            outcome(50, Some(85), None, false),
            outcome(50, Some(85), None, false),
            outcome(100, None, Some(SkipReason::QualityNotMet), false),
            outcome(100, None, None, true),
        ];
        assert_eq!(point_savings(&strata, &samples), 2_500);
        let (depth, quality, not_smaller, failed, failed_bytes) =
            cumulative_skips(&strata, &samples);
        assert_eq!(depth.count, 0);
        assert_eq!(quality.count, 25);
        assert_eq!(not_smaller.count, 0);
        assert_eq!(failed, 25);
        assert_eq!(failed_bytes, 2_500);
        assert!(bootstrap_margin(&strata, &samples, 50) > 0);
    }

    #[test]
    fn curve_accumulates_savings_in_bpp_order() {
        let mut strata: [StratumTotals; STRATUM_COUNT] =
            std::array::from_fn(|_| Default::default());
        strata[0] = totals(100, 1_000);
        strata[1] = totals(100, 3_000);
        let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
        let sample = |new_bytes: u64| SampleOutcome {
            old_bytes: 100,
            new_bytes,
            quality: Some(85),
            skip: None,
            failed: false,
            source_pixels: 1_000,
            read_secs: 0.0,
            bad_pixel_ratio: 0.0,
        };
        samples[0].push(sample(50));
        samples[1].push(sample(90));
        let curve = benefit_curve(&strata, &samples, 100);
        assert_eq!(curve.len(), 3);
        assert_eq!(curve[1].coverage, 0.75);
        assert_eq!(curve[1].savings_bytes, 300);
        assert_eq!(curve[1].elapsed_seconds, 50);
        assert_eq!(curve[2].coverage, 1.0);
        assert_eq!(curve[2].savings_bytes, 800);
        // 最后一点的耗时就是整次执行的总预计耗时。
        assert_eq!(curve[2].elapsed_seconds, 100);
    }

    #[test]
    fn estimate_is_read_only() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "估算测试", 1).unwrap();
        let mut ids = Vec::new();
        let mut original_hashes = Vec::new();
        for index in 0..3 {
            let mut image = RgbImage::new(64, 64);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                let value = ((x * 31 + y * 17 + index * 7) % 256) as u8;
                *pixel = Rgb([value, value.wrapping_mul(3), value.wrapping_mul(5)]);
            }
            let mut bytes = Vec::new();
            DynamicImage::ImageRgb8(image)
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            fs::write(root.join(format!("{hash}.png")), &bytes).unwrap();
            let id = session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, 'png', 1, ?2, 64, 64, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 2, '保留', 1, 1)",
                        params![hash, bytes.len() as i64],
                    )?;
                    Ok(connection.last_insert_rowid())
                })
                .unwrap();
            ids.push(id);
            original_hashes.push((hash, bytes.len() as i64));
        }

        let before: Vec<(i64, String, i64, i64)> = session
            .db
            .with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT id, content_hash, file_size, modified_at FROM images ORDER BY id",
                )?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();

        let options = OptimizeOptions::default();
        let estimate = estimate(
            &session.db,
            &root,
            "estimate-test",
            &ids,
            &options,
            10,
            "fingerprint",
            &Recorder::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(estimate.total_count, 3);
        assert_eq!(estimate.candidate_count, 3);
        assert!(estimate.sample_count > 0);

        let after: Vec<(i64, String, i64, i64)> = session
            .db
            .with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT id, content_hash, file_size, modified_at FROM images ORDER BY id",
                )?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(before, after);
        let operations: i64 =
            session
                .db
                .with_reader(|connection| {
                    Ok(connection
                        .query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?)
                })
                .unwrap();
        assert_eq!(operations, 0);
        let temp_dir = root.join(".artbank").join("temp");
        assert_eq!(fs::read_dir(temp_dir).unwrap().count(), 0);
        for (hash, size) in original_hashes {
            let path = root.join(format!("{hash}.png"));
            assert_eq!(fs::metadata(path).unwrap().len(), size as u64);
        }
    }
}

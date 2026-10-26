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

use rayon::prelude::*;
use serde::Serialize;

use crate::{
    app::{error::AppResult, tasks::TaskScope},
    imageops::{
        colors,
        optimize::{self, LadderOutcome, OptimizeFormat, OptimizeOptions, SkipReason, SourceInfo},
        phash, thumbnail,
    },
    import::decode,
    library::{
        BatchReporter,
        db::LibraryDb,
        encode_pool, metadata,
        optimize::{CandidateRow, collect_candidates},
        paths::LibraryPaths,
        service::now_ms,
    },
};

const BUCKET_BOUNDS: [f64; 5] = [0.10, 0.15, 0.22, 0.32, 0.50];
const BUCKET_COUNT: usize = 6;
/// 有损源 / 无损源两组 × 6 个 bpp 桶。
const STRATUM_COUNT: usize = BUCKET_COUNT * 2;
const MIN_PER_STRATUM: usize = 3;
const BOOTSTRAP_ROUNDS: usize = 300;
const MAX_SAMPLE_FAILURES: usize = 20;
const CHUNK: usize = 400;

pub const SAMPLE_MIN: usize = 50;
pub const SAMPLE_MAX: usize = 2000;
pub const SAMPLE_STEP: usize = 50;

/// 批次 1 的写入侧保守常数：真实执行反馈要到批次 2 才用于校准。
/// 写入带宽按顺序写单张文件估；`sync_all` 每张一次；提交按每 1000 张一次。
const WRITE_BYTES_PER_SEC: f64 = 150.0 * 1024.0 * 1024.0;
const FSYNC_SECS_PER_IMAGE: f64 = 0.004;
const COMMIT_SECS_PER_BATCH: f64 = 0.08;
const COMMIT_BATCH_SIZE: u64 = 1000;
const DELETE_SECS_PER_IMAGE: f64 = 0.0008;

/// 抽样数量必须是范围内的整步长；前后端共用这组边界。
pub fn sample_size_valid(size: usize) -> bool {
    (SAMPLE_MIN..=SAMPLE_MAX).contains(&size) && size.is_multiple_of(SAMPLE_STEP)
}

/// 统计候选只占进度条前 25%：这一段只按 id 读数据库行，样本编码才是耗时主体。
const SCAN_PERCENT: u8 = 25;
/// 编码样本从 26% 走到 100%。
const SAMPLE_PERCENT_START: u8 = SCAN_PERCENT + 1;

/// 统计阶段的百分比；超过总数（例如传入的 id 有已删除行）也不会超过 `SCAN_PERCENT`。
fn scan_percentage(scanned: usize, scan_total: usize) -> u8 {
    if scan_total == 0 {
        return SCAN_PERCENT;
    }
    ((scanned.min(scan_total) * usize::from(SCAN_PERCENT)) / scan_total) as u8
}

/// 编码样本的百分比；完成全部样本时正好 100。
fn sample_percentage(completed: usize, sample_total: usize) -> u8 {
    if sample_total == 0 {
        return 100;
    }
    let span = usize::from(100 - SAMPLE_PERCENT_START);
    (usize::from(SAMPLE_PERCENT_START) + completed.min(sample_total) * span / sample_total) as u8
}

/// 收尾文案与满进度：汇总、外推、构造结果都在 100% 之后，不能让用户以为卡住了。
fn finish_progress(reporter: &dyn BatchReporter) {
    reporter.message("正在汇总结果…");
    reporter.percent(100);
}

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

/// 前置判据（只看数据库列）拦下的图片统计。
///
/// 执行阶段遇到同样的原因只是跳过，不会出现在报告里；估算要把它们算进“跳过”才不误导。
#[derive(Debug, Clone, Default)]
struct PrefilterSkips {
    compact: SkipBucket,
    optimized: SkipBucket,
    transparent: SkipBucket,
    too_large: SkipBucket,
    animated: SkipBucket,
}

impl PrefilterSkips {
    /// 记一张被前置判据拦下的图片；不是前置判据的原因（16 位、画质、体积）返回 `false`。
    fn record(&mut self, reason: &SkipReason, bytes: u64) -> bool {
        match reason {
            SkipReason::BppLow => self.compact.add(1, bytes),
            SkipReason::AlreadyOptimized => self.optimized.add(1, bytes),
            SkipReason::Transparent => self.transparent.add(1, bytes),
            SkipReason::TooLargeForWebp => self.too_large.add(1, bytes),
            SkipReason::AnimatedOrMultipage => self.animated.add(1, bytes),
            _ => return false,
        }
        true
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

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeEstimate {
    pub task_id: String,
    pub created_at: i64,
    pub options_fingerprint: String,
    /// 估算用的参数与范围：接管任务时据此重建执行请求，不依赖前端内存。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<OptimizeOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<TaskScope>,
    pub scope_label: String,
    pub sample_size: u64,
    /// 发起估算时的图库标识；换库后不接管旧任务。
    pub library_id: String,
    /// 实际参与编码的样本张数与原始字节数；含失败样本。
    pub sample_count: u64,
    pub sample_bytes: u64,
    pub coverage: f64,
    /// 样本编码阶段的墙钟耗时（毫秒），供抽样提示与后续校准使用。
    pub sample_elapsed_ms: u64,
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
    pub skip_too_large: SkipBucket,
    /// 前置跳过：GIF（动画/多帧保守跳过）。
    pub skip_animated: SkipBucket,
    pub skip_color_profile: SkipBucket,
    pub skip_depth: SkipBucket,
    pub skip_quality: SkipBucket,
    pub skip_not_smaller: SkipBucket,
    /// 候选内跳过：APNG、动画 WebP、多页 TIFF。
    pub skip_multiframe: SkipBucket,
    /// 候选内跳过：数据库列与实际文件不符（实际有 alpha、实际尺寸更大、实际字节更多），
    /// 前置判据到解码后才发现，所以没进「未纳入候选」。
    pub skip_late: SkipBucket,
    pub failed_count: u64,
    /// 失败样本按比例外推出的体积；与上面的跳过分类一起构成“跳过”总量。
    pub failed_bytes: u64,
    pub savings_bytes: u64,
    pub savings_margin_bytes: u64,
    pub predicted_bytes: u64,
    pub estimated_seconds: u64,
    /// 读取、解码、阶梯编码、哈希与重算的耗时。
    pub estimated_work_seconds: u64,
    /// 写新文件与逐张 `sync_all` 的耗时（批次 1 为保守模型）。
    pub estimated_write_seconds: u64,
    /// 按批提交与删旧图、删预览的耗时（批次 1 为保守模型）。
    pub estimated_finish_seconds: u64,
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
struct SampleOutcome {
    old_bytes: u64,
    new_bytes: u64,
    quality: Option<u8>,
    skip: Option<SkipReason>,
    failed: bool,
    source_pixels: u64,
    read_secs: f64,
    /// 解码 + 阶梯编码 + 回读比对；不含读取、哈希与重算。
    encode_secs: f64,
    /// 成功编码后算 BLAKE3 的耗时。
    hash_secs: f64,
    /// 缩略图 / pHash / 主色的重算耗时（按勾选执行）。
    recalc_secs: f64,
    bad_pixel_ratio: f64,
}

/// 候选内跳过的全部落点；`process` 由候选减去它们得到，保证漏斗闭合。
#[derive(Debug, Clone, Default)]
struct SkipTotals {
    color_profile: SkipBucket,
    depth: SkipBucket,
    quality: SkipBucket,
    not_smaller: SkipBucket,
    multiframe: SkipBucket,
    late: SkipBucket,
    failed_count: u64,
    failed_bytes: u64,
}

impl SkipTotals {
    fn count(&self) -> u64 {
        self.color_profile.count
            + self.depth.count
            + self.quality.count
            + self.not_smaller.count
            + self.multiframe.count
            + self.late.count
            + self.failed_count
    }

    fn bytes(&self) -> u64 {
        self.color_profile.bytes
            + self.depth.bytes
            + self.quality.bytes
            + self.not_smaller.bytes
            + self.multiframe.bytes
            + self.late.bytes
            + self.failed_bytes
    }

    /// 外推后各桶之和可能略超候选总量；按比例收窄，保证
    /// `candidate = process + skip` 严格成立。
    fn fit_within(&mut self, candidate_count: u64, candidate_bytes: u64) {
        let mut counts = [
            self.color_profile.count,
            self.depth.count,
            self.quality.count,
            self.not_smaller.count,
            self.multiframe.count,
            self.late.count,
            self.failed_count,
        ];
        fit_to_total(&mut counts, candidate_count);
        [
            &mut self.color_profile.count,
            &mut self.depth.count,
            &mut self.quality.count,
            &mut self.not_smaller.count,
            &mut self.multiframe.count,
            &mut self.late.count,
        ]
        .into_iter()
        .zip(counts)
        .for_each(|(slot, value)| *slot = value);
        self.failed_count = counts[6];

        let mut bytes = [
            self.color_profile.bytes,
            self.depth.bytes,
            self.quality.bytes,
            self.not_smaller.bytes,
            self.multiframe.bytes,
            self.late.bytes,
            self.failed_bytes,
        ];
        fit_to_total(&mut bytes, candidate_bytes);
        [
            &mut self.color_profile.bytes,
            &mut self.depth.bytes,
            &mut self.quality.bytes,
            &mut self.not_smaller.bytes,
            &mut self.multiframe.bytes,
            &mut self.late.bytes,
        ]
        .into_iter()
        .zip(bytes)
        .for_each(|(slot, value)| *slot = value);
        self.failed_bytes = bytes[6];
    }
}

/// 把一组数按比例压到 `limit` 以内；总和本来不超限时原样保留。
fn fit_to_total(values: &mut [u64], limit: u64) {
    let sum: u64 = values.iter().sum();
    if sum <= limit {
        return;
    }
    if limit == 0 || sum == 0 {
        values.fill(0);
        return;
    }
    let mut scaled: Vec<u64> = values
        .iter()
        .map(|value| (*value as u128 * limit as u128 / sum as u128) as u64)
        .collect();
    let mut rest = limit - scaled.iter().sum::<u64>();
    // 余数按原值从大到小补，避免小桶吃掉全部余量。
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by_key(|index| std::cmp::Reverse(values[*index]));
    for index in order {
        if rest == 0 {
            break;
        }
        scaled[index] += 1;
        rest -= 1;
    }
    values.copy_from_slice(&scaled);
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
    let mut skips = PrefilterSkips::default();

    // 统计阶段只报百分比、不报张数：这一段的“张数”就是整个范围，当成抽样进度会让人
    // 以为在重跑全库。真正能按张数计的只有样本编码。
    let scan_total = image_ids.len();
    reporter.message("正在统计候选图片…");
    reporter.report(0, 0);
    reporter.percent(scan_percentage(0, scan_total));
    let mut scanned = 0_usize;
    for chunk in image_ids.chunks(CHUNK) {
        if reporter.cancelled() {
            return Ok(None);
        }
        let rows = collect_candidates(database, chunk)?;
        scanned += chunk.len();
        for row in rows {
            total_count += 1;
            let bytes = row.file_size.max(0) as u64;
            total_bytes += bytes;
            // 与执行阶段共用同一段前置判据；这里只有数据库列，位深按 8 位占位。
            let source = SourceInfo {
                format: row.format.clone(),
                file_size: row.file_size,
                width: row.width,
                height: row.height,
                has_alpha: row.has_alpha,
                optimized_at: row.optimized_at,
                bit_depth: 8,
            };
            if let Err(reason) = optimize::prefilter(&source, options)
                && skips.record(&reason, bytes)
            {
                continue;
            }
            let stratum = stratum_index(row.lossless_source(), bucket_for(source.bpp()));
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
        reporter.percent(scan_percentage(scanned, scan_total));
    }

    let candidate_count: u64 = totals.iter().map(|stratum| stratum.count).sum();
    let candidate_bytes: u64 = totals.iter().map(|stratum| stratum.bytes).sum();
    let candidate_pixels: u64 = totals.iter().map(|stratum| stratum.pixels).sum();
    if candidate_count == 0 {
        finish_progress(reporter);
        return Ok(Some(empty_estimate(
            task_id,
            options_fingerprint,
            total_count,
            total_bytes,
            &skips,
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
    reporter.message("正在挑选样本…");
    reporter.report(0, sample_total);
    reporter.percent(SAMPLE_PERCENT_START);
    if sample_total == 0 {
        finish_progress(reporter);
        return Ok(Some(empty_estimate(
            task_id,
            options_fingerprint,
            total_count,
            total_bytes,
            &skips,
        )));
    }

    let sample_ids: Vec<i64> = sample_tasks.iter().map(|(_, id, _)| *id).collect();
    let sample_rows: HashMap<i64, CandidateRow> = collect_candidates(database, &sample_ids)?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();

    let pool = encode_pool("估算")?;
    let threads = pool.current_num_threads();

    reporter.message("正在编码样本…");
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
                reporter.percent(sample_percentage(completed, sample_total));
                Some((*stratum, *id, outcome, error))
            })
            .collect::<Vec<_>>()
    });
    let elapsed = started.elapsed().as_secs_f64();
    if reporter.cancelled() {
        return Ok(None);
    }

    // 样本编码完还有汇总、外推与构造结果；先把进度推满并换文案，避免看起来卡在最后一张。
    reporter.report(sample_total, sample_total);
    finish_progress(reporter);

    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    let mut sample_bytes = 0_u64;
    let mut sample_pixels = 0_u64;
    let mut read_secs = 0.0_f64;
    let mut work_secs = 0.0_f64;
    let mut sample_failures = Vec::new();
    let mut bad_pixel_sum = 0.0_f64;
    let mut bad_pixel_count = 0_u64;
    for (stratum, id, outcome, error) in records {
        sample_bytes += outcome.old_bytes;
        sample_pixels += outcome.source_pixels;
        read_secs += outcome.read_secs;
        work_secs += outcome.encode_secs + outcome.hash_secs + outcome.recalc_secs;
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
    let mut skip_totals = cumulative_skips(&totals, &samples);
    // 外推后的各桶可能略超候选总量；先收窄再算 process，漏斗才不会出现负数或重复。
    skip_totals.fit_within(candidate_count, candidate_bytes);
    let process_count = candidate_count - skip_totals.count();
    let process_bytes = candidate_bytes - skip_totals.bytes();
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

    let timing = estimate_seconds(
        elapsed,
        read_secs,
        work_secs,
        threads,
        sample_bytes,
        candidate_pixels,
        candidate_bytes,
        process_count,
        process_bytes,
        savings_bytes,
        sample_pixels,
    );

    Ok(Some(OptimizeEstimate {
        task_id: task_id.to_owned(),
        created_at: now_ms(),
        options_fingerprint: options_fingerprint.to_owned(),
        options: Some(options.clone()),
        // 范围与图库标识由 `AppState` 在写入缓存前补齐。
        scope: None,
        scope_label: String::new(),
        // 记录所选档位（而不是实际抽到的张数），界面据此判断候选是否不足。
        sample_size: sample_budget as u64,
        library_id: String::new(),
        sample_count: sample_total as u64,
        sample_bytes,
        coverage: sample_bytes as f64 / candidate_bytes.max(1) as f64,
        sample_elapsed_ms: (elapsed * 1000.0).round() as u64,
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
        skip_compact: skips.compact.clone(),
        skip_optimized: skips.optimized.clone(),
        skip_transparent: skips.transparent.clone(),
        skip_too_large: skips.too_large.clone(),
        skip_animated: skips.animated.clone(),
        skip_color_profile: skip_totals.color_profile,
        skip_depth: skip_totals.depth,
        skip_quality: skip_totals.quality,
        skip_not_smaller: skip_totals.not_smaller,
        skip_multiframe: skip_totals.multiframe,
        skip_late: skip_totals.late,
        failed_count: skip_totals.failed_count,
        failed_bytes: skip_totals.failed_bytes,
        savings_bytes,
        savings_margin_bytes,
        predicted_bytes,
        estimated_seconds: timing.total(),
        estimated_work_seconds: timing.work,
        estimated_write_seconds: timing.write,
        estimated_finish_seconds: timing.finish,
        quality_counts,
        curve: benefit_curve(&totals, &samples, timing.work, timing.write, timing.finish),
        folders: folder_estimates,
        sample_failures,
    }))
}

fn empty_estimate(
    task_id: &str,
    fingerprint: &str,
    total_count: u64,
    total_bytes: u64,
    skips: &PrefilterSkips,
) -> OptimizeEstimate {
    OptimizeEstimate {
        task_id: task_id.to_owned(),
        created_at: now_ms(),
        options_fingerprint: fingerprint.to_owned(),
        options: None,
        scope: None,
        scope_label: String::new(),
        sample_size: 0,
        library_id: String::new(),
        sample_count: 0,
        sample_bytes: 0,
        coverage: 0.0,
        sample_elapsed_ms: 0,
        sample_bad_pixel_ratio: 0.0,
        total_count,
        total_bytes,
        candidate_count: 0,
        candidate_bytes: 0,
        process_count: 0,
        process_bytes: 0,
        skip_compact: skips.compact.clone(),
        skip_optimized: skips.optimized.clone(),
        skip_transparent: skips.transparent.clone(),
        skip_too_large: skips.too_large.clone(),
        skip_animated: skips.animated.clone(),
        skip_color_profile: SkipBucket::default(),
        skip_depth: SkipBucket::default(),
        skip_quality: SkipBucket::default(),
        skip_not_smaller: SkipBucket::default(),
        skip_multiframe: SkipBucket::default(),
        skip_late: SkipBucket::default(),
        failed_count: 0,
        failed_bytes: 0,
        savings_bytes: 0,
        savings_margin_bytes: 0,
        predicted_bytes: total_bytes,
        estimated_seconds: 0,
        estimated_work_seconds: 0,
        estimated_write_seconds: 0,
        estimated_finish_seconds: 0,
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
        encode_secs: 0.0,
        hash_secs: 0.0,
        recalc_secs: 0.0,
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
    let old_bytes = source_bytes.len() as u64;
    let skip_now = |skip: SkipReason| SampleOutcome {
        old_bytes,
        new_bytes: old_bytes,
        quality: None,
        skip: Some(skip),
        failed: false,
        source_pixels: row.pixels(),
        read_secs,
        encode_secs: 0.0,
        hash_secs: 0.0,
        recalc_secs: 0.0,
        bad_pixel_ratio: 0.0,
    };
    if optimize::multiframe_guard(&source_bytes, &row.format).is_err() {
        return Ok(skip_now(SkipReason::AnimatedOrMultipage));
    }
    if optimize::color_profile_guard(&source_bytes, &row.format, options).is_err() {
        return Ok(skip_now(SkipReason::ColorProfile));
    }

    let encode_started = Instant::now();
    let decoded = decode::decode(&path)?;

    let source = SourceInfo {
        format: row.format.clone(),
        file_size: source_bytes.len() as i64,
        width: decoded.width,
        height: decoded.height,
        has_alpha: row.has_alpha || decoded.has_alpha,
        optimized_at: row.optimized_at,
        bit_depth: optimize::bit_depth(&decoded.image),
    };
    let source_pixels = u64::from(decoded.width) * u64::from(decoded.height);

    let plan = match optimize::plan(&source, options) {
        Ok(plan) => plan,
        Err(reason) => {
            return Ok(SampleOutcome {
                old_bytes,
                new_bytes: old_bytes,
                quality: None,
                skip: Some(reason),
                failed: false,
                source_pixels,
                read_secs,
                encode_secs: encode_started.elapsed().as_secs_f64(),
                hash_secs: 0.0,
                recalc_secs: 0.0,
                bad_pixel_ratio: 0.0,
            });
        }
    };
    let prepared = optimize::prepare(decoded.image, options.max_long_edge);
    let source_jpeg = matches!(row.format.as_str(), "jpeg" | "jpg")
        .then_some(source_bytes.as_slice())
        .filter(|_| options.output_format == OptimizeFormat::Jpeg);
    let ladder = optimize::encode_ladder(&prepared, &plan, options, old_bytes, source_jpeg)?;
    let encode_secs = encode_started.elapsed().as_secs_f64();

    // 样本也真实执行哈希与三个重算，只丢弃结果，用耗时校准预计耗时。
    let mut hash_secs = 0.0;
    let mut recalc_secs = 0.0;
    let encoded = match &ladder {
        LadderOutcome::Encoded(candidate) => {
            let hash_started = Instant::now();
            let _ = blake3::hash(&candidate.bytes);
            hash_secs = hash_started.elapsed().as_secs_f64();
            let recalc_started = Instant::now();
            if options.regenerate_thumbnails {
                let _ = thumbnail::encode(&prepared);
            }
            if options.recompute_phash {
                let _ = phash::compute(&prepared);
            }
            if options.recompute_colors {
                let _ = colors::dominant(&prepared);
            }
            recalc_secs = recalc_started.elapsed().as_secs_f64();
            Some(candidate)
        }
        LadderOutcome::Skipped(_) => None,
    };

    let bad_pixel_ratio = encoded
        .as_ref()
        .map_or(0.0, |candidate| candidate.bad_pixel_ratio);
    let (quality, new_bytes, skip) = match (encoded, &ladder) {
        (Some(candidate), _) => (Some(candidate.quality), candidate.bytes.len() as u64, None),
        (None, LadderOutcome::Skipped(reason)) => (None, old_bytes, Some(*reason)),
        (None, LadderOutcome::Encoded(_)) => unreachable!("已在上面的分支里取出编码结果"),
    };
    Ok(SampleOutcome {
        old_bytes,
        new_bytes,
        quality,
        skip,
        failed: false,
        source_pixels,
        read_secs,
        encode_secs,
        hash_secs,
        recalc_secs,
        bad_pixel_ratio,
    })
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
            // 节省率是“新/旧”的比例，上限是 1.0；`MAX_SIZE_RATIO` 是替换阈值，
            // 不是节省率上限，用它夹会把 PNG→JPEG 这类高压缩比图低估。
            rates[index] = (1.0 - new as f64 / old as f64).clamp(0.0, 1.0);
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
    work_seconds: u64,
    write_seconds: u64,
    finish_seconds: u64,
) -> Vec<CurvePoint> {
    let rates = stratum_rates(totals, samples);
    let candidate_bytes: u64 = totals.iter().map(|stratum| stratum.bytes).sum();
    if candidate_bytes == 0 {
        return Vec::new();
    }
    let candidate_pixels: u64 = totals.iter().map(|stratum| stratum.pixels).sum();
    let candidate_count: u64 = totals.iter().map(|stratum| stratum.count).sum();
    let output_bytes: u64 = totals
        .iter()
        .zip(rates.iter())
        .map(|(stratum, rate)| (stratum.bytes as f64 * (1.0 - rate)).round() as u64)
        .sum();
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
    let mut counts = 0_u64;
    let mut outputs = 0_u64;
    let mut savings = 0_u64;
    for index in order {
        processed += totals[index].bytes;
        pixels += totals[index].pixels;
        counts += totals[index].count;
        savings += (totals[index].bytes as f64 * rates[index]).round() as u64;
        outputs += (totals[index].bytes as f64 * (1.0 - rates[index])).round() as u64;
        // 三段耗时各自的权重不同：编码按像素、写盘按预测输出字节、删除按张数。
        let work = share(work_seconds, pixels, candidate_pixels);
        let write = share(write_seconds, outputs, output_bytes);
        let finish = share(finish_seconds, counts, candidate_count);
        points.push(CurvePoint {
            coverage: processed as f64 / candidate_bytes as f64,
            savings_bytes: savings,
            elapsed_seconds: work + write + finish,
        });
    }
    points
}

/// 按权重分摊耗时；权重为 0 或总量为 0 时返回 0。
fn share(total: u64, part: u64, whole: u64) -> u64 {
    if total == 0 || whole == 0 || part == 0 {
        return 0;
    }
    (total as f64 * part as f64 / whole as f64).round() as u64
}

/// 把所有 `SkipReason` 归到候选内跳过桶；没有落点的原因一律进 `late`，
/// 保证 `process = candidate - Σskip` 不会把张数算多。
fn cumulative_skips(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
) -> SkipTotals {
    let mut result = SkipTotals::default();
    for (index, outcomes) in samples.iter().enumerate() {
        let total = &totals[index];
        if total.count == 0 || outcomes.is_empty() {
            continue;
        }
        let sample_count = outcomes.len() as u64;
        let mut depth_samples = 0_u64;
        let mut quality_samples = 0_u64;
        let mut not_smaller_samples = 0_u64;
        let mut color_samples = 0_u64;
        let mut multiframe_samples = 0_u64;
        let mut late_samples = 0_u64;
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
                Some(SkipReason::ColorProfile) => color_samples += 1,
                Some(SkipReason::AnimatedOrMultipage) => multiframe_samples += 1,
                // 扫描时用的 `has_alpha`、宽高与字节数来自数据库列，解码后换成
                // 真实值；两者不一致时（实际有 alpha、实际尺寸更大）前置判据会
                // 在这里才失败，单列一桶，保证漏斗不缺口。
                Some(_) => late_samples += 1,
                None => {}
            }
        }
        let add = |bucket: &mut SkipBucket, part: u64| {
            bucket.add(
                scale(total.count, part, sample_count),
                scale(total.bytes, part, sample_count),
            );
        };
        add(&mut result.depth, depth_samples);
        add(&mut result.quality, quality_samples);
        add(&mut result.not_smaller, not_smaller_samples);
        add(&mut result.color_profile, color_samples);
        add(&mut result.multiframe, multiframe_samples);
        add(&mut result.late, late_samples);
        result.failed_count += scale(total.count, failed_samples, sample_count);
        result.failed_bytes += scale(total.bytes, failed_samples, sample_count);
    }
    result
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
                total += stratum.bytes as f64 * rate.clamp(0.0, 1.0);
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

/// 预计耗时的三段拆分；批次 1 的写盘、提交与删除都用保守常数。
#[derive(Debug, Clone, Copy, Default)]
struct TimeBreakdown {
    work: u64,
    write: u64,
    finish: u64,
}

impl TimeBreakdown {
    fn total(self) -> u64 {
        self.work + self.write + self.finish
    }
}

#[allow(clippy::too_many_arguments)]
fn estimate_seconds(
    elapsed: f64,
    read_secs: f64,
    sample_work_secs: f64,
    threads: usize,
    sample_bytes: u64,
    candidate_pixels: u64,
    candidate_bytes: u64,
    process_count: u64,
    process_bytes: u64,
    savings_bytes: u64,
    sample_pixels: u64,
) -> TimeBreakdown {
    if candidate_pixels == 0 || sample_pixels == 0 {
        return TimeBreakdown::default();
    }
    // 样本墙钟已经包含读取、解码、编码、哈希与重算，按像素外推。
    let wall = elapsed * candidate_pixels as f64 / sample_pixels as f64;
    // 分段耗时只算编码/哈希/重算，按线程数折算成墙钟；墙钟更保守，两者取大。
    let segments = if threads == 0 {
        0.0
    } else {
        sample_work_secs / threads as f64 * candidate_pixels as f64 / sample_pixels as f64
    };
    let cpu = wall.max(segments);
    let io = if read_secs > 0.001 && sample_bytes > 0 && candidate_bytes > 0 {
        // 采样读取时间按线程数折算成聚合带宽，与执行阶段使用同一档线程数。
        let bandwidth = sample_bytes as f64 * threads as f64 / read_secs;
        candidate_bytes as f64 / bandwidth
    } else {
        0.0
    };
    let work = cpu.max(io);
    // 跳过图不写盘，所以写盘字节按“处理体积 − 预测节省”算，再计每张一次 fsync。
    let output_bytes = process_bytes.saturating_sub(savings_bytes) as f64;
    let write = output_bytes / WRITE_BYTES_PER_SEC + process_count as f64 * FSYNC_SECS_PER_IMAGE;
    // 提交按批、删除按张。
    let batches = process_count.div_ceil(COMMIT_BATCH_SIZE) as f64;
    let finish = batches * COMMIT_SECS_PER_BATCH + process_count as f64 * DELETE_SECS_PER_IMAGE;
    TimeBreakdown {
        work: work.round() as u64,
        write: write.round() as u64,
        finish: finish.round() as u64,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor};

    use image::{DynamicImage, Rgb, RgbImage};
    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::{BatchReporter, service};

    #[derive(Default)]
    struct Recorder {
        completed: std::sync::atomic::AtomicUsize,
        percent: std::sync::atomic::AtomicU8,
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

        fn percent(&self, percent: u8) {
            self.percent
                .store(percent, std::sync::atomic::Ordering::Relaxed);
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
    fn sample_size_accepts_only_range_and_step_multiples() {
        assert!(sample_size_valid(50));
        assert!(sample_size_valid(200));
        assert!(sample_size_valid(2000));
        assert!(!sample_size_valid(49));
        assert!(!sample_size_valid(55));
        assert!(!sample_size_valid(2050));
    }

    #[test]
    fn progress_percentages_stay_inside_their_phase() {
        // 统计阶段只走到 25%，越界（含已删除的行）也不会越过它。
        assert_eq!(scan_percentage(0, 390_542), 0);
        assert_eq!(scan_percentage(390_542, 390_542), SCAN_PERCENT);
        assert_eq!(scan_percentage(400_000, 390_542), SCAN_PERCENT);
        // 编码样本从 26% 起步，正好在最后一张到 100%。
        assert_eq!(sample_percentage(0, 200), SAMPLE_PERCENT_START);
        assert_eq!(sample_percentage(200, 200), 100);
        assert!(sample_percentage(100, 200) > scan_percentage(390_542, 390_542));
        assert_eq!(sample_percentage(0, 0), 100);
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
                    encode_secs: 0.0,
                    hash_secs: 0.0,
                    recalc_secs: 0.0,
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
        let skips = cumulative_skips(&strata, &samples);
        assert_eq!(skips.color_profile.count, 0);
        assert_eq!(skips.depth.count, 0);
        assert_eq!(skips.quality.count, 25);
        assert_eq!(skips.not_smaller.count, 0);
        assert_eq!(skips.multiframe.count, 0);
        assert_eq!(skips.late.count, 0);
        assert_eq!(skips.failed_count, 25);
        assert_eq!(skips.failed_bytes, 2_500);
        // 候选 100 张 = 处理 50 + 跳过 50，漏斗闭合。
        assert_eq!(skips.count() + 50, 100);
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
            encode_secs: 0.0,
            hash_secs: 0.0,
            recalc_secs: 0.0,
            bad_pixel_ratio: 0.0,
        };
        samples[0].push(sample(50));
        samples[1].push(sample(90));
        // 三段耗时都只由像素/输出字节/张数决定，这里给 60/30/10 便于断言。
        let curve = benefit_curve(&strata, &samples, 60, 30, 10);
        assert_eq!(curve.len(), 3);
        assert_eq!(curve[1].coverage, 0.75);
        assert_eq!(curve[1].savings_bytes, 300);
        assert_eq!(curve[2].coverage, 1.0);
        assert_eq!(curve[2].savings_bytes, 800);
        // 最后一点的耗时就是整次执行的总预计耗时。
        assert_eq!(curve[2].elapsed_seconds, 100);
        assert!(curve[1].elapsed_seconds > 0 && curve[1].elapsed_seconds < 100);
    }

    #[test]
    fn savings_rate_is_not_capped_by_replace_threshold() {
        let mut strata: [StratumTotals; STRATUM_COUNT] =
            std::array::from_fn(|_| Default::default());
        strata[0] = totals(1, 1_000);
        let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
        samples[0].push(SampleOutcome {
            old_bytes: 1_000,
            new_bytes: 20,
            quality: Some(75),
            skip: None,
            failed: false,
            source_pixels: 1_000,
            read_secs: 0.0,
            encode_secs: 0.0,
            hash_secs: 0.0,
            recalc_secs: 0.0,
            bad_pixel_ratio: 0.0,
        });
        let rates = stratum_rates(&strata, &samples);
        // 压到原图 2% 时节省率应是 98%，不能被 0.95 的替换阈值夹住。
        assert!((rates[0] - 0.98).abs() < 1e-9, "{:?}", rates[0]);
    }

    #[test]
    fn skip_buckets_fit_within_candidate_totals() {
        let mut skips = SkipTotals {
            quality: SkipBucket {
                count: 80,
                bytes: 800,
            },
            multiframe: SkipBucket {
                count: 40,
                bytes: 400,
            },
            failed_count: 30,
            failed_bytes: 300,
            ..SkipTotals::default()
        };
        // 外推后各桶之和（150 张）超过候选（100 张），收窄到候选以内。
        skips.fit_within(100, 1_000);
        assert_eq!(skips.count(), 100);
        assert_eq!(skips.bytes(), 1_000);
    }

    #[test]
    fn fit_to_total_keeps_zero_and_small_sets() {
        let mut values = [1_u64, 2, 3];
        fit_to_total(&mut values, 100);
        assert_eq!(values, [1, 2, 3]);
        let mut values = [1_u64, 2, 3];
        fit_to_total(&mut values, 0);
        assert_eq!(values, [0, 0, 0]);
        let mut values = [10_u64, 0, 0];
        fit_to_total(&mut values, 4);
        assert_eq!(values.iter().sum::<u64>(), 4);
        assert_eq!(values[0], 4);
    }

    /// 一条估算里同时出现前置跳过、候选内跳过、动画/多页与失败，
    /// 四个不变量必须严格成立。
    #[test]
    fn funnel_invariants_hold_with_animated_and_failed_samples() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "漏斗测试", 1).unwrap();

        let insert = |format: &str, ext: &str, bytes: &[u8], width: u32, height: u32| {
            let hash = blake3::hash(bytes).to_hex().to_string();
            if !bytes.is_empty() {
                fs::write(root.join(format!("{hash}.{ext}")), bytes).unwrap();
            }
            session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 2, '', 1, 1)",
                        params![hash, ext, bytes.len() as i64, width, height, format],
                    )?;
                    Ok(connection.last_insert_rowid())
                })
                .unwrap()
        };

        let mut ids = Vec::new();
        // 三张正常 PNG：进候选。
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
            ids.push(insert("png", "png", &bytes, 64, 64));
        }
        // GIF：只看数据库列就跳过，不需要真实文件。
        ids.push(insert("gif", "gif", &[], 64, 64));
        // 假 APNG：带 acTL，候选内跳过。
        let mut apng = b"\x89PNG\r\n\x1a\n".to_vec();
        apng.extend_from_slice(&0_u32.to_be_bytes());
        apng.extend_from_slice(b"acTL");
        apng.extend_from_slice(&[0, 0, 0, 0]);
        apng.resize(256, 0);
        ids.push(insert("png", "png", &apng, 64, 64));
        // 行还在但文件缺失：读取失败。
        ids.push(insert("png", "png", &[7_u8; 256], 64, 64));

        let reporter = Recorder::default();
        let estimate = estimate(
            &session.db,
            &root,
            "funnel",
            &ids,
            &OptimizeOptions::default(),
            50,
            "fingerprint",
            &reporter,
        )
        .unwrap()
        .unwrap();

        let prefilter_count = estimate.skip_compact.count
            + estimate.skip_optimized.count
            + estimate.skip_transparent.count
            + estimate.skip_too_large.count
            + estimate.skip_animated.count;
        let prefilter_bytes = estimate.skip_compact.bytes
            + estimate.skip_optimized.bytes
            + estimate.skip_transparent.bytes
            + estimate.skip_too_large.bytes
            + estimate.skip_animated.bytes;
        let candidate_skip_count = estimate.skip_color_profile.count
            + estimate.skip_depth.count
            + estimate.skip_quality.count
            + estimate.skip_not_smaller.count
            + estimate.skip_multiframe.count
            + estimate.skip_late.count
            + estimate.failed_count;
        let candidate_skip_bytes = estimate.skip_color_profile.bytes
            + estimate.skip_depth.bytes
            + estimate.skip_quality.bytes
            + estimate.skip_not_smaller.bytes
            + estimate.skip_multiframe.bytes
            + estimate.skip_late.bytes
            + estimate.failed_bytes;

        assert_eq!(
            estimate.total_count,
            prefilter_count + estimate.candidate_count,
            "{estimate:?}"
        );
        assert_eq!(
            estimate.total_bytes,
            prefilter_bytes + estimate.candidate_bytes,
            "{estimate:?}"
        );
        assert_eq!(
            estimate.candidate_count,
            estimate.process_count + candidate_skip_count,
            "{estimate:?}"
        );
        assert_eq!(
            estimate.candidate_bytes,
            estimate.process_bytes + candidate_skip_bytes,
            "{estimate:?}"
        );

        // GIF 进前置桶，APNG 与失败进候选内桶。
        assert_eq!(estimate.skip_animated.count, 1);
        assert_eq!(estimate.skip_multiframe.count, 1);
        assert_eq!(estimate.failed_count, 1);
        // 档位 50 > 候选 5，结果里记的是档位而不是实际张数。
        assert_eq!(estimate.sample_size, 50);
        assert_eq!(estimate.sample_count, 5);
        assert!(estimate.sample_elapsed_ms > 0 || estimate.sample_count == 0);
        assert_eq!(
            estimate.estimated_seconds,
            estimate.estimated_work_seconds
                + estimate.estimated_write_seconds
                + estimate.estimated_finish_seconds
        );
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
        let reporter = Recorder::default();
        let estimate = estimate(
            &session.db,
            &root,
            "estimate-test",
            &ids,
            &options,
            10,
            "fingerprint",
            &reporter,
        )
        .unwrap()
        .unwrap();
        assert_eq!(estimate.total_count, 3);
        assert_eq!(estimate.candidate_count, 3);
        assert!(estimate.sample_count > 0);
        assert_eq!(
            reporter.percent.load(std::sync::atomic::Ordering::Relaxed),
            100,
            "估算结束时进度应到 100%"
        );

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

use std::{
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    concurrency::{
        budget::{BudgetError, ImageBudget, ImageMemoryBudget, estimate_working_bytes},
        resources::{HolderInfo, ResourceTable},
    },
    imageops::{
        colors,
        optimize::{self, LadderOutcome, OptimizeFormat, OptimizeOptions, SkipReason, SourceInfo},
        phash, thumbnail,
    },
    import::decode,
    library::{BatchReporter, content, db::LibraryDb, optimize::CandidateRow},
};

use super::{
    report::failed_outcome,
    strata::{STRATUM_COUNT, StratumTotals},
};

pub(super) const MIN_PER_STRATUM: usize = 3;
/// 统计候选只占进度条前 25%：这一段只按 id 读数据库行，样本编码才是耗时主体。
pub(super) const SCAN_PERCENT: u8 = 25;
/// 编码样本从 26% 走到 100%。
pub(super) const SAMPLE_PERCENT_START: u8 = SCAN_PERCENT + 1;

/// 统计阶段的百分比；超过总数（例如传入的 id 有已删除行）也不会超过 `SCAN_PERCENT`。
pub(super) fn scan_percentage(scanned: usize, scan_total: usize) -> u8 {
    if scan_total == 0 {
        return SCAN_PERCENT;
    }
    ((scanned.min(scan_total) * usize::from(SCAN_PERCENT)) / scan_total) as u8
}

/// 编码样本的百分比；完成全部样本时正好 100。
pub(super) fn sample_percentage(completed: usize, sample_total: usize) -> u8 {
    if sample_total == 0 {
        return 100;
    }
    let span = usize::from(100 - SAMPLE_PERCENT_START);
    (usize::from(SAMPLE_PERCENT_START) + completed.min(sample_total) * span / sample_total) as u8
}

/// 收尾文案与满进度：汇总、外推、构造结果都在 100% 之后，不能让用户以为卡住了。
pub(super) fn finish_progress(reporter: &dyn BatchReporter) {
    reporter.message("正在汇总结果…");
    reporter.percent(100);
}

/// 扫描阶段留下的候选：层内要按累计体积切片，所以必须逐张保留字节数。
#[derive(Debug, Clone, Copy)]
pub(super) struct StratumEntry {
    pub(super) id: i64,
    pub(super) bytes: u64,
    pub(super) pixels: u64,
}

/// 一张样本，以及它代表的整份切片。
///
/// 外推按切片权重走，不再假设“层内按张数均匀”：切片等体积，样本的体积与像素
/// 构成由构造贴合总体，节省率与耗时都不再依赖抽到的是哪一批图。
#[derive(Debug, Clone, Copy)]
pub(super) struct SampleTask {
    pub(super) stratum: usize,
    pub(super) id: i64,
    /// 数据库行已被删除时用它兜底像素。
    pub(super) db_pixels: u64,
    pub(super) weight_bytes: u64,
    pub(super) weight_count: u64,
}

#[derive(Debug, Clone)]
pub(super) struct SampleOutcome {
    pub(super) old_bytes: u64,
    pub(super) new_bytes: u64,
    /// 这张样本代表的候选体积与张数（整份切片）；由抽样阶段回填。
    pub(super) weight_bytes: u64,
    pub(super) weight_count: u64,
    pub(super) quality: Option<u8>,
    pub(super) skip: Option<SkipReason>,
    pub(super) failed: bool,
    /// 失败原因；成功时为 `None`。
    pub(super) error: Option<String>,
    pub(super) source_pixels: u64,
    pub(super) read_secs: f64,
    /// 解码 + 阶梯编码 + 回读比对；不含读取、哈希与重算。
    pub(super) encode_secs: f64,
    /// 成功编码后算 BLAKE3 的耗时。
    pub(super) hash_secs: f64,
    /// 缩略图 / pHash / 主色的重算耗时（按勾选执行）。
    pub(super) recalc_secs: f64,
    pub(super) bad_pixel_ratio: f64,
}

pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
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

    pub(super) fn below(&mut self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        (self.next() % bound as u64) as usize
    }
}

pub(super) fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ u64::from(std::process::id()).wrapping_mul(0x9E37_79B9)
}

/// 编码一个样本；失败也返回 `SampleOutcome`，把已经发生的读取/编码耗时带回去。
#[allow(clippy::too_many_arguments)]
pub(super) fn sample_one(
    database: &LibraryDb,
    root: &Path,
    row: &CandidateRow,
    options: &OptimizeOptions,
    resources: &ResourceTable,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
    holder: &HolderInfo,
    cancel: &dyn Fn() -> bool,
) -> SampleOutcome {
    let read_started = Instant::now();
    let (current, source_bytes) =
        match content::read_original(database, resources, root, row.id, holder, cancel) {
            Ok(value) => value,
            Err(error) => {
                return failed_outcome(
                    row.pixels(),
                    row.file_size.max(0) as u64,
                    read_started.elapsed().as_secs_f64(),
                    format!("无法读取图片：{error}"),
                );
            }
        };
    // 路径读锁已在 `read_original` 返回前释放，再拿预算做解码与编码。
    let _permit = match images.acquire(cancel) {
        Ok(permit) => permit,
        Err(BudgetError::Cancelled) => {
            return failed_outcome(
                row.pixels(),
                row.file_size.max(0) as u64,
                read_started.elapsed().as_secs_f64(),
                "估算已取消".to_owned(),
            );
        }
    };
    let read_secs = read_started.elapsed().as_secs_f64();
    // 线程许可已在上面拿到；字节额度按这张图的像素与源体积估算，压缩路径口径与执行阶段一致。
    let estimate = estimate_working_bytes(row.width, row.height, row.file_size.max(0) as u64, true);
    let _memory = match memory.acquire(estimate, cancel) {
        Ok(permit) => permit,
        Err(BudgetError::Cancelled) => {
            return failed_outcome(
                row.pixels(),
                row.file_size.max(0) as u64,
                read_secs,
                "估算已取消".to_owned(),
            );
        }
    };
    let old_bytes = source_bytes.len() as u64;
    let skip_now = |skip: SkipReason| SampleOutcome {
        old_bytes,
        new_bytes: old_bytes,
        weight_bytes: 0,
        weight_count: 0,
        quality: None,
        skip: Some(skip),
        failed: false,
        error: None,
        source_pixels: row.pixels(),
        read_secs,
        encode_secs: 0.0,
        hash_secs: 0.0,
        recalc_secs: 0.0,
        bad_pixel_ratio: 0.0,
    };
    if optimize::multiframe_guard(&source_bytes, &current.format).is_err() {
        return skip_now(SkipReason::AnimatedOrMultipage);
    }
    if optimize::color_profile_guard(&source_bytes, &current.format, options).is_err() {
        return skip_now(SkipReason::ColorProfile);
    }

    let encode_started = Instant::now();
    // 已经读过一次盘：解码直接用这份字节，不再按路径读第二遍。
    let decoded = match decode::decode_bytes(&source_bytes, Some(&current.file_ext)) {
        Ok(decoded) => decoded,
        Err(error) => {
            return failed_outcome(row.pixels(), old_bytes, read_secs, error.to_string());
        }
    };

    let source = SourceInfo {
        format: current.format.clone(),
        file_size: source_bytes.len() as i64,
        width: decoded.width,
        height: decoded.height,
        has_alpha: current.has_alpha || decoded.has_alpha,
        optimized_at: row.optimized_at,
        bit_depth: optimize::bit_depth(&decoded.image),
    };
    let source_pixels = u64::from(decoded.width) * u64::from(decoded.height);

    let plan = match optimize::plan(&source, options) {
        Ok(plan) => plan,
        Err(reason) => {
            return SampleOutcome {
                old_bytes,
                new_bytes: old_bytes,
                weight_bytes: 0,
                weight_count: 0,
                quality: None,
                skip: Some(reason),
                failed: false,
                error: None,
                source_pixels,
                read_secs,
                encode_secs: encode_started.elapsed().as_secs_f64(),
                hash_secs: 0.0,
                recalc_secs: 0.0,
                bad_pixel_ratio: 0.0,
            };
        }
    };
    let prepared = optimize::prepare(decoded.image, options.max_long_edge);
    let source_jpeg = matches!(row.format.as_str(), "jpeg" | "jpg")
        .then_some(source_bytes.as_slice())
        .filter(|_| options.output_format == OptimizeFormat::Jpeg);
    let ladder = match optimize::encode_ladder(&prepared, &plan, options, old_bytes, source_jpeg) {
        Ok(ladder) => ladder,
        Err(error) => {
            return failed_outcome(source_pixels, old_bytes, read_secs, error.to_string());
        }
    };
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
    SampleOutcome {
        old_bytes,
        new_bytes,
        weight_bytes: 0,
        weight_count: 0,
        quality,
        skip,
        failed: false,
        error: None,
        source_pixels,
        read_secs,
        encode_secs,
        hash_secs,
        recalc_secs,
        bad_pixel_ratio,
    }
}

/// 按候选体积分配样本；每个有候选的层至少 3 张，余量按字节份额分配。
pub(super) fn allocate_samples(
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

/// 层内抽样：按累计体积把该层切成至多 `take` 份，每份随机取一张代表整份。
///
/// 返回 `(下标, 该份字节, 该份张数)`，各份字节之和恒等于层体积、张数之和恒等于
/// 层张数，所以外推不需要再按张数等比例放大。层内候选不足 `take` 时退化为全取。
///
/// 层内按张数等概率抽会让样本的体积构成变成一次独立的重尾抽样，节省率与耗时
/// 外推会被同时带偏。这里按“最后一个字节落在第几份”归组：等体积切片让样本的
/// 体积与像素构成由构造贴合总体；某张图独占若干份时它会自成一份并被必然抽到，
/// 尾部不会整体缺席。
pub(super) fn slice_indices(
    entries: &[StratumEntry],
    take: usize,
    rng: &mut Rng,
) -> Vec<(usize, u64, u64)> {
    let length = entries.len();
    let take = take.min(length);
    if take == 0 {
        return Vec::new();
    }
    if take == length {
        return entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, entry.bytes, 1))
            .collect();
    }
    let total: u128 = entries.iter().map(|entry| entry.bytes as u128).sum();
    // 整层都是 0 字节（极少见）：按张数等分，权重仍然精确。
    let weight_of =
        |entry: &StratumEntry| -> u128 { if total == 0 { 1 } else { entry.bytes as u128 } };
    let total_weight: u128 = entries.iter().map(weight_of).sum();

    let mut order: Vec<usize> = (0..length).collect();
    order.sort_unstable_by_key(|index| (weight_of(&entries[*index]), entries[*index].id));

    let mut groups: Vec<(Vec<usize>, u64, u64)> = vec![(Vec::new(), 0, 0); take];
    let mut cumulative = 0_u128;
    for index in order {
        cumulative += weight_of(&entries[index]);
        // 用“最后一个字节”的位置归组；0 字节的成员不推进累计量，所以这里要防下溢。
        let group =
            ((cumulative.saturating_sub(1)) * take as u128 / total_weight).min((take - 1) as u128);
        let slot = &mut groups[group as usize];
        slot.0.push(index);
        slot.1 += entries[index].bytes;
        slot.2 += 1;
    }
    let mut result = Vec::with_capacity(take);
    for (members, bytes, count) in groups {
        if members.is_empty() {
            // 某张图占满多份时会出现空份：它已经由那一份代表，不再重复计数。
            continue;
        }
        result.push((members[rng.below(members.len())], bytes, count));
    }
    result
}

/// 单张样本的节省率；跳过与失败都按“零节省”计。
pub(super) fn sample_rate(sample: &SampleOutcome) -> f64 {
    if sample.old_bytes == 0 {
        return 0.0;
    }
    // 节省率是“新/旧”的比例，上限是 1.0；`MAX_SIZE_RATIO` 是替换阈值，
    // 不是节省率上限，用它夹会把 PNG→JPEG 这类高压缩比图低估。
    (1.0 - sample.new_bytes as f64 / sample.old_bytes as f64).clamp(0.0, 1.0)
}

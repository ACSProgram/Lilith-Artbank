use crate::{imageops::optimize::SkipReason, library::calibration::OptimizeCalibration};

use super::{
    CurvePoint,
    report::SkipTotals,
    sampling::{Rng, SampleOutcome, sample_rate},
    strata::{BUCKET_COUNT, STRATUM_COUNT, StratumTotals, stratum_rates},
};

/// 批次 1 的写入侧保守常数；批次 2 起优先用本库上次执行的实测校准值
/// （`meta.optimize.calibration.v1`），没有记录时才退回这些常数。
const COMMIT_BATCH_SIZE: u64 = crate::library::optimize::BATCH_SIZE as u64;

pub(super) const BOOTSTRAP_ROUNDS: usize = 300;

/// 把一组数按比例压到 `limit` 以内；总和本来不超限时原样保留。
pub(super) fn fit_to_total(values: &mut [u64], limit: u64) {
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

pub(super) fn point_savings(
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

pub(super) fn benefit_curve(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
    work_seconds: u64,
    write_seconds: u64,
    finish_seconds: u64,
) -> Vec<CurvePoint> {
    let rates = stratum_rates(totals, samples);
    let candidate_bytes: u64 = totals.iter().map(|stratum| stratum.bytes).sum();
    let candidate_pixels: u64 = totals.iter().map(|stratum| stratum.pixels).sum();
    let candidate_count: u64 = totals.iter().map(|stratum| stratum.count).sum();
    // 横轴按张数走，张数为 0 时覆盖率没有意义。
    if candidate_bytes == 0 || candidate_count == 0 {
        return Vec::new();
    }
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
    let mut pixels = 0_u64;
    let mut counts = 0_u64;
    let mut outputs = 0_u64;
    let mut savings = 0_u64;
    for index in order {
        pixels += totals[index].pixels;
        counts += totals[index].count;
        savings += (totals[index].bytes as f64 * rates[index]).round() as u64;
        outputs += (totals[index].bytes as f64 * (1.0 - rates[index])).round() as u64;
        // 三段耗时各自的权重不同：编码按像素、写盘按预测输出字节、删除按张数。
        let work = share(work_seconds, pixels, candidate_pixels);
        let write = share(write_seconds, outputs, output_bytes);
        let finish = share(finish_seconds, counts, candidate_count);
        points.push(CurvePoint {
            // 横轴是已处理的候选张数占比，与执行时的进度条同源；体积分布极不均匀
            // （最高 bpp 那层 35% 的张数占七成体积），任何体积轴都会与进度错位。
            coverage: counts as f64 / candidate_count as f64,
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
///
/// 每张样本带的是它代表的整份切片，所以这里直接累加权重，不再按张数等比例放大。
pub(super) fn cumulative_skips(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
) -> SkipTotals {
    let mut result = SkipTotals::default();
    for (index, outcomes) in samples.iter().enumerate() {
        if totals[index].count == 0 || outcomes.is_empty() {
            continue;
        }
        for sample in outcomes {
            let (count, bytes) = (sample.weight_count, sample.weight_bytes);
            if sample.failed {
                result.failed_count += count;
                result.failed_bytes += bytes;
                continue;
            }
            match sample.skip {
                Some(SkipReason::SixteenBit) => result.depth.add(count, bytes),
                Some(SkipReason::QualityNotMet) => result.quality.add(count, bytes),
                Some(SkipReason::NotSmallEnough) => result.not_smaller.add(count, bytes),
                Some(SkipReason::ColorProfile) => result.color_profile.add(count, bytes),
                Some(SkipReason::AnimatedOrMultipage) => result.multiframe.add(count, bytes),
                // 扫描时用的 `has_alpha`、宽高与字节数来自数据库列，解码后换成
                // 真实值；两者不一致时（实际有 alpha、实际尺寸更大）前置判据会
                // 在这里才失败，单列一桶，保证漏斗不缺口。
                Some(_) => result.late.add(count, bytes),
                None => {}
            }
        }
    }
    result
}

/// 节省空间与耗时的误差区间。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Margins {
    pub(super) savings: u64,
    pub(super) seconds: u64,
}

/// 以“切片”为单位重抽：切片等体积，方差估计才有意义。
///
/// 抽样就是全体时不存在抽样误差，直接返回 0——旧实现即使在覆盖 100%、估算与
/// 实际逐字节相等时仍报 ±8%…±18%，那是重抽本身产生的方差，不是误差。
pub(super) fn margins(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
    rounds: usize,
    inputs: &TimingInputs,
    timing: TimeBreakdown,
    candidate_count: u64,
    sample_count: u64,
) -> Margins {
    if rounds == 0 || sample_count == 0 || sample_count >= candidate_count {
        return Margins::default();
    }
    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    let mut savings = Vec::with_capacity(rounds);
    let mut seconds = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let mut round_savings = 0.0_f64;
        let mut round = WorkTotals::default();
        for (index, outcomes) in samples.iter().enumerate() {
            if totals[index].count == 0 || outcomes.is_empty() {
                continue;
            }
            let count = outcomes.len();
            let mut work = 0.0_f64;
            let mut pixels = 0.0_f64;
            for _ in 0..count {
                let sample = &outcomes[rng.below(count)];
                round_savings += sample.weight_bytes as f64 * sample_rate(sample);
                round.total_secs +=
                    sample.read_secs + sample.encode_secs + sample.hash_secs + sample.recalc_secs;
                work += sample.encode_secs + sample.hash_secs + sample.recalc_secs;
                pixels += sample.source_pixels as f64;
            }
            round.work_secs += work;
            round.pixels += pixels;
            if pixels > 0.0 && totals[index].pixels > 0 {
                round.scaled_work += work * (totals[index].pixels as f64 / pixels);
            }
        }
        savings.push(round_savings);
        // 逐张累加得到的是串行 CPU 秒，点估计用的是并行墙钟。不折算的话区间会围
        // 绕「串行总和」展开，比点估计大出一个线程数倍。
        round.total_secs /= inputs.threads.max(1) as f64;
        seconds.push(inputs.work(&round) + timing.write as f64 + timing.finish as f64);
    }
    let work_margin = 1.96 * deviation(&seconds);
    // 写入与提交侧没有抽样，用校准值的 ±50% 当固定项，与抽样波动按平方和合成。
    let fixed = 0.5 * (timing.write + timing.finish) as f64;
    Margins {
        savings: (1.96 * deviation(&savings)).round() as u64,
        seconds: (work_margin * work_margin + fixed * fixed).sqrt().round() as u64,
    }
}

fn deviation(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f64>()
        / values.len() as f64;
    variance.sqrt()
}

/// 预计耗时的三段拆分；写入侧优先用本库实测校准值，没有记录时用保守常数。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TimeBreakdown {
    pub(super) work: u64,
    pub(super) write: u64,
    pub(super) finish: u64,
}

impl TimeBreakdown {
    pub(super) fn total(self) -> u64 {
        self.work + self.write + self.finish
    }
}

/// 一次抽样（或一次重抽）的耗时统计。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct WorkTotals {
    /// 读取 + 编码 + 哈希 + 重算的秒数。点估计直接换成实测墙钟；区间重抽是逐张
    /// 串行累加的，用前要按线程数折算成墙钟（见 `margins`）。
    pub(super) total_secs: f64,
    /// 只有编码 + 哈希 + 重算的秒数。
    pub(super) work_secs: f64,
    /// 按各层自己的像素比放大后的 CPU 秒数。
    pub(super) scaled_work: f64,
    /// 样本像素总量。
    pub(super) pixels: f64,
}

impl WorkTotals {
    pub(super) fn collect(
        totals: &[StratumTotals; STRATUM_COUNT],
        samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
    ) -> Self {
        let mut result = WorkTotals::default();
        for (index, outcomes) in samples.iter().enumerate() {
            let mut work = 0.0_f64;
            let mut secs = 0.0_f64;
            let mut pixels = 0.0_f64;
            for sample in outcomes {
                secs +=
                    sample.read_secs + sample.encode_secs + sample.hash_secs + sample.recalc_secs;
                work += sample.encode_secs + sample.hash_secs + sample.recalc_secs;
                pixels += sample.source_pixels as f64;
            }
            result.total_secs += secs;
            result.work_secs += work;
            result.pixels += pixels;
            // 每层各自按自己的像素比放大：样本量按体积分配，高 bpp 层拿到的样本
            // 远多于它的像素份额，用全局像素比会把耗时整体放大一倍以上。
            if pixels > 0.0 && totals[index].pixels > 0 {
                result.scaled_work += work * (totals[index].pixels as f64 / pixels);
            }
        }
        result
    }
}

/// 耗时外推的固定输入：候选总量与采样带宽。区间重抽时只换样本侧。
pub(super) struct TimingInputs {
    pub(super) read_secs: f64,
    pub(super) threads: usize,
    pub(super) sample_bytes: u64,
    pub(super) candidate_pixels: u64,
    pub(super) candidate_bytes: u64,
}

impl TimingInputs {
    /// 把一次抽样的实测耗时外推到全库。
    fn work(&self, measured: &WorkTotals) -> f64 {
        if self.candidate_pixels == 0 || measured.pixels <= 0.0 {
            return 0.0;
        }
        let scale = if measured.work_secs > 0.0 {
            measured.scaled_work / measured.work_secs
        } else {
            self.candidate_pixels as f64 / measured.pixels
        };
        // 样本墙钟已经包含读取、解码、编码、哈希与重算，按同一个倍率外推。
        let wall = measured.total_secs * scale;
        // 分段耗时只算编码/哈希/重算，按线程数折算成墙钟；墙钟更保守，两者取大。
        let segments = if self.threads == 0 {
            0.0
        } else {
            measured.scaled_work / self.threads as f64
        };
        let cpu = wall.max(segments);
        let io = if self.read_secs > 0.001 && self.sample_bytes > 0 && self.candidate_bytes > 0 {
            // 采样读取时间按线程数折算成聚合带宽，与执行阶段使用同一档线程数。
            let bandwidth = self.sample_bytes as f64 * self.threads as f64 / self.read_secs;
            self.candidate_bytes as f64 / bandwidth
        } else {
            0.0
        };
        cpu.max(io)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn estimate_seconds(
    inputs: &TimingInputs,
    measured: &WorkTotals,
    process_count: u64,
    process_bytes: u64,
    savings_bytes: u64,
    calibration: OptimizeCalibration,
) -> TimeBreakdown {
    if inputs.candidate_pixels == 0 || measured.pixels <= 0.0 {
        return TimeBreakdown::default();
    }
    let work = inputs.work(measured);
    // 跳过图不写盘，所以写盘字节按“处理体积 − 预测节省”算，再计每张的 fsync 成本。
    let output_bytes = process_bytes.saturating_sub(savings_bytes) as f64;
    let write = output_bytes * calibration.write_secs_per_byte
        + process_count as f64 * calibration.fsync_secs_per_image;
    // 提交按批、删除按张。
    let batches = process_count.div_ceil(COMMIT_BATCH_SIZE) as f64;
    let finish = batches * calibration.commit_secs_per_batch
        + process_count as f64 * calibration.delete_secs_per_image;
    TimeBreakdown {
        work: work.round() as u64,
        write: write.round() as u64,
        finish: finish.round() as u64,
    }
}

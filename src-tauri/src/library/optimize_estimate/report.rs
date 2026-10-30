use crate::{imageops::optimize::SkipReason, library::service::now_ms};

use super::{OptimizeEstimate, SkipBucket, extrapolate::fit_to_total, sampling::SampleOutcome};

/// 前置判据（只看数据库列）拦下的图片统计。
///
/// 执行阶段遇到同样的原因只是跳过，不会出现在报告里；估算要把它们算进“跳过”才不误导。
#[derive(Debug, Clone, Default)]
pub(super) struct PrefilterSkips {
    pub(super) compact: SkipBucket,
    pub(super) optimized: SkipBucket,
    pub(super) transparent: SkipBucket,
    pub(super) too_large: SkipBucket,
    pub(super) animated: SkipBucket,
}

impl PrefilterSkips {
    /// 记一张被前置判据拦下的图片；不是前置判据的原因（16 位、画质、体积）返回 `false`。
    pub(super) fn record(&mut self, reason: &SkipReason, bytes: u64) -> bool {
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

/// 候选内跳过的全部落点；`process` 由候选减去它们得到，保证漏斗闭合。
#[derive(Debug, Clone, Default)]
pub(super) struct SkipTotals {
    pub(super) color_profile: SkipBucket,
    pub(super) depth: SkipBucket,
    pub(super) quality: SkipBucket,
    pub(super) not_smaller: SkipBucket,
    pub(super) multiframe: SkipBucket,
    pub(super) late: SkipBucket,
    pub(super) failed_count: u64,
    pub(super) failed_bytes: u64,
}

impl SkipTotals {
    pub(super) fn count(&self) -> u64 {
        self.color_profile.count
            + self.depth.count
            + self.quality.count
            + self.not_smaller.count
            + self.multiframe.count
            + self.late.count
            + self.failed_count
    }

    pub(super) fn bytes(&self) -> u64 {
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
    pub(super) fn fit_within(&mut self, candidate_count: u64, candidate_bytes: u64) {
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

pub(super) fn empty_estimate(
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
        candidate_fingerprint: String::new(),
        options: None,
        scope: None,
        scope_label: String::new(),
        sample_size: 0,
        library_id: String::new(),
        sample_count: 0,
        sample_bytes: 0,
        coverage: 0.0,
        sample_large_count: 0,
        candidate_large_count: 0,
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
        estimated_seconds_margin: 0,
        estimated_work_seconds: 0,
        estimated_write_seconds: 0,
        estimated_finish_seconds: 0,
        quality_counts: Vec::new(),
        curve: Vec::new(),
        folders: Vec::new(),
        sample_failures: Vec::new(),
    }
}

pub(super) fn failed_outcome(
    pixels: u64,
    bytes: u64,
    read_secs: f64,
    error: String,
) -> SampleOutcome {
    SampleOutcome {
        old_bytes: bytes,
        new_bytes: bytes,
        weight_bytes: 0,
        weight_count: 0,
        quality: None,
        skip: None,
        failed: true,
        error: Some(error),
        source_pixels: pixels,
        // 读取已经发生的时间要留下：全 0 会让 IO 模型偏乐观。
        read_secs,
        encode_secs: 0.0,
        hash_secs: 0.0,
        recalc_secs: 0.0,
        bad_pixel_ratio: 0.0,
    }
}

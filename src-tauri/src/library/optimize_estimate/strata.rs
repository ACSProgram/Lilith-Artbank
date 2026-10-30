use super::sampling::{SampleOutcome, sample_rate};

const BUCKET_BOUNDS: [f64; 5] = [0.10, 0.15, 0.22, 0.32, 0.50];
pub(super) const BUCKET_COUNT: usize = 6;
/// 有损源 / 无损源两组 × 6 个 bpp 桶。
pub(super) const STRATUM_COUNT: usize = BUCKET_COUNT * 2;

#[derive(Debug, Clone, Default)]
pub(super) struct StratumTotals {
    pub(super) count: u64,
    pub(super) bytes: u64,
    pub(super) pixels: u64,
}

pub(super) fn bucket_for(bpp: f64) -> usize {
    BUCKET_BOUNDS
        .iter()
        .position(|bound| bpp < *bound)
        .unwrap_or(BUCKET_BOUNDS.len())
}

pub(super) fn stratum_index(lossless: bool, bucket: usize) -> usize {
    usize::from(lossless) * BUCKET_COUNT + bucket.min(BUCKET_COUNT - 1)
}

pub(super) fn stratum_rates(
    totals: &[StratumTotals; STRATUM_COUNT],
    samples: &[Vec<SampleOutcome>; STRATUM_COUNT],
) -> [f64; STRATUM_COUNT] {
    let mut rates = [0.0_f64; STRATUM_COUNT];
    for (index, outcomes) in samples.iter().enumerate() {
        if totals[index].count == 0 || outcomes.is_empty() {
            continue;
        }
        let weight: u64 = outcomes.iter().map(|sample| sample.weight_bytes).sum();
        if weight > 0 {
            // 切片等体积，按切片字节加权就是这个层的字节加权节省率。
            let weighted: f64 = outcomes
                .iter()
                .map(|sample| sample.weight_bytes as f64 * sample_rate(sample))
                .sum();
            rates[index] = (weighted / weight as f64).clamp(0.0, 1.0);
            continue;
        }
        // 整层都是 0 字节：退化成按张数加权。
        let sum: f64 = outcomes.iter().map(sample_rate).sum();
        rates[index] = (sum / outcomes.len() as f64).clamp(0.0, 1.0);
    }
    rates
}

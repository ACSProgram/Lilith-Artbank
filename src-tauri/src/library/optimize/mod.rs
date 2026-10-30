//! 图片空间优化的写路径：候选收集、凭证校验、分批编码发布与旧图清理。
//!
//! 每张图先编码到统一临时目录；达到判据后按固定顺序发布：
//! 文件数据 fsync → 持久化 rename → 目录 flush → SQLite 事务提交（FULL）→ 删旧图。
//! 提交返回之前绝不删旧图；提交失败则删掉本批已发布的新文件。
//! 压缩不可撤销：记录只在时间线上占位并说明原图已删除。

mod candidates;
mod commit;
mod execute;

#[cfg(test)]
mod tests;

use serde::Serialize;

use crate::imageops::optimize::SkipReason;

pub use candidates::{
    CandidateRow, candidate_fingerprint, collect_candidates, eligible_summary, fingerprint_ids,
    verify_grant, verify_sources,
};
pub use execute::optimize;
// 只有测试里的基准直接调用它。
#[cfg(test)]
pub(crate) use execute::optimize_with_batch_size;

/// 每批编码完成后在同一事务里发布，限制内存峰值与事务时长。
///
/// 256 张是双缓冲下的默认切分；字节上限避免少量超大图撑爆一批的内存峰值。
pub(crate) const BATCH_SIZE: usize = 256;
/// 原图缺失或大小不符的允许比例；超过就拒绝执行，要求重新估算。
pub(crate) const SOURCE_DRIFT_LIMIT: f64 = 0.01;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeFailure {
    pub image_id: i64,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeReport {
    pub processed: u64,
    /// 全部跳过数（前置 + 候选内 + 内容重复），等于三个细分字段之和。
    pub skipped: u64,
    /// 只看数据库列就拦下的张数（GIF、bpp 低、已处理、透明、超 WebP 尺寸）。
    pub prefilter_skipped: u64,
    /// 解码或编码后才发现不处理的张数（色彩、16 位、画质、省得太少、动画/多页）。
    pub candidate_skipped: u64,
    /// 候选内跳过按原因的六个桶；之和恒等于 `candidate_skipped`。
    pub skip_color_profile: u64,
    pub skip_not_smaller: u64,
    pub skip_quality: u64,
    pub skip_depth: u64,
    pub skip_multiframe: u64,
    /// 其余原因：解码后才发现 bpp 太低、带透明、超出 WebP 尺寸等。
    pub skip_late: u64,
    /// 目标内容已被其他行占用而没有落盘的张数。
    pub duplicate_skipped: u64,
    pub failed: u64,
    /// 原图在编码前就找不到或读不出来：不当作解码/磁盘故障，也不触发连续失败停止。
    pub source_missing: u64,
    pub old_bytes: u64,
    pub new_bytes: u64,
    pub savings_bytes: u64,
    /// 连续失败过多而提前停止；字符串说明原因。
    pub stop_reason: Option<String>,
    pub cancelled: bool,
    pub failures: Vec<OptimizeFailure>,
    /// 整次执行的墙钟耗时。
    pub elapsed_ms: u64,
    /// 解码 + 阶梯编码 + 回读比对。
    pub encode_ms: u64,
    /// 成功编码后计算 BLAKE3。
    pub hash_ms: u64,
    /// 缩略图 / pHash / 主色重算（按勾选执行）。
    pub recalc_ms: u64,
    /// 写临时文件（含 fsync）。
    pub write_ms: u64,
    /// 其中 `sync_all` 的部分，用于校准每张 fsync 成本。
    pub fsync_ms: u64,
    /// 持久化 rename 与目录 flush。
    pub publish_ms: u64,
    /// 事务提交（含缩略图追加与行更新）。
    pub commit_ms: u64,
    /// 删旧图与查看器预览。
    pub delete_ms: u64,
    pub deleted_files: u64,
    pub delete_failed: u64,
}

impl OptimizeReport {
    /// 记一次候选内跳过：总数与细分桶一起加，两个数不会各说一套。
    fn record_skip(&mut self, reason: SkipReason) {
        match reason {
            SkipReason::ColorProfile => self.skip_color_profile += 1,
            SkipReason::NotSmallEnough => self.skip_not_smaller += 1,
            SkipReason::QualityNotMet => self.skip_quality += 1,
            SkipReason::SixteenBit => self.skip_depth += 1,
            SkipReason::AnimatedOrMultipage => self.skip_multiframe += 1,
            SkipReason::BppLow
            | SkipReason::AlreadyOptimized
            | SkipReason::Transparent
            | SkipReason::TooLargeForWebp => self.skip_late += 1,
        }
        self.candidate_skipped += 1;
    }

    /// 候选内的跳过数（含内容重复与原图缺失），不含执行前被排除的图片。
    ///
    /// 这是任务、界面与曲线唯一的跳过口径；`skipped` 里那部分执行前排除的张数
    /// 不属于候选池，混进来会让分母与分子对不上。
    pub fn progress_skipped(&self) -> usize {
        (self.candidate_skipped + self.duplicate_skipped + self.source_missing) as usize
    }

    /// 跳过细分的对外形状：只保留非 0 桶，按张数降序；key 与估算视图的图例一致。
    pub fn skip_buckets(&self) -> Vec<(String, u64)> {
        let mut buckets = vec![
            ("color".to_owned(), self.skip_color_profile),
            ("depth".to_owned(), self.skip_depth),
            ("quality".to_owned(), self.skip_quality),
            ("notSmaller".to_owned(), self.skip_not_smaller),
            ("multiframe".to_owned(), self.skip_multiframe),
            ("late".to_owned(), self.skip_late),
        ];
        buckets.retain(|(_, count)| *count > 0);
        buckets.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        buckets
    }
}

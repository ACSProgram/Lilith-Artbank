//! 图片空间优化的只读估算：分层抽样、真实编码、外推与分文件夹明细。
//!
//! 估算不写数据库、不碰原文件、不产生临时文件。样本按「有损源 / 无损源 × bpp 桶」
//! 分层，跨层按候选体积分配样本量、层内按体积等分切片，避免抽样系统性低估高 bpp
//! 的图。

use serde::Serialize;

use crate::{app::tasks::TaskScope, imageops::optimize::OptimizeOptions};

mod estimate;
mod extrapolate;
mod report;
mod sampling;
mod strata;

#[cfg(test)]
mod tests;

pub use estimate::estimate;

pub const SAMPLE_MIN: usize = 50;
pub const SAMPLE_MAX: usize = 2000;
pub const SAMPLE_STEP: usize = 50;

/// 抽样数量必须是范围内的整步长；前后端共用这组边界。
pub fn sample_size_valid(size: usize) -> bool {
    (SAMPLE_MIN..=SAMPLE_MAX).contains(&size) && size.is_multiple_of(SAMPLE_STEP)
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkipBucket {
    pub count: u64,
    pub bytes: u64,
}

impl SkipBucket {
    pub(super) fn add(&mut self, count: u64, bytes: u64) {
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
    /// 累计耗时：work 按像素、write 按该层预测输出字节、finish 按该层张数，
    /// 最后一点等于总预计耗时。
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
    /// 估算时候选集合的指纹；执行前比对，挡住“张数与体积都对、但换了一批图”。
    pub candidate_fingerprint: String,
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
    /// 样本里 ≥ 32 MP 的张数（按解码后尺寸）；候选那一侧按数据库宽高，两边同阈值。
    pub sample_large_count: u64,
    pub candidate_large_count: u64,
    /// 样本编码阶段的墙钟耗时（毫秒），界面用它折算抽样大概要等多久。
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
    /// 预计耗时的误差区间（±秒）；覆盖 100% 时为 0。
    pub estimated_seconds_margin: u64,
    /// 读取、解码、阶梯编码、哈希与重算的耗时。
    pub estimated_work_seconds: u64,
    /// 写新文件与逐张 fsync 的耗时；优先用本库上次执行的实测校准。
    pub estimated_write_seconds: u64,
    /// 按批提交与删旧图、删预览的耗时；优先用本库上次执行的实测校准。
    pub estimated_finish_seconds: u64,
    pub quality_counts: Vec<QualityCount>,
    /// 按 bpp 从高到低执行时的累计节省；x 轴是候选体积占比。
    pub curve: Vec<CurvePoint>,
    pub folders: Vec<FolderEstimate>,
    pub sample_failures: Vec<String>,
}

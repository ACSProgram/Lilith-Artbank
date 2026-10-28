use crate::app::error::{AppError, AppResult};

pub mod calibration;
pub mod capability;
pub mod collections;
pub mod content;
pub mod db;
pub mod edit;
pub mod folders;
pub mod history;
pub mod import;
pub mod inspect;
pub mod maintenance_actions;
pub mod manifest;
pub mod metadata;
pub mod optimize;
pub mod optimize_estimate;
pub mod pack_gc;
pub mod paths;
pub mod query;
pub mod schema;
pub mod service;
pub mod similarity;
pub mod stats;
pub mod temp;
pub mod thumbs;
pub mod viewer;

#[cfg(test)]
mod perf;

/// 生成 `?, ?, ?` 形式的占位符串，供按 id 分块的 `IN (...)` 语句复用。
///
/// 分块大小由调用方决定（通常 400）；SQLite 默认变量上限是 32766，不要一次传入整批 id。
pub(crate) fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 批量文件操作的进度与取消回调；由 app 层的任务句柄实现，测试用 `NoopReporter`。
pub trait BatchReporter: Send + Sync {
    fn report(&self, completed: usize, total: usize);
    fn cancelled(&self) -> bool;
    /// 可选的进度文案；不关心文案的调用方用默认实现。
    fn message(&self, _message: &str) {}
    /// 可选的百分比：阶段之间单位不同（读行 vs 编码）时用它给进度条加权。
    fn percent(&self, _percent: u8) {}
    /// 可选的加权进度计量器；任务句柄提供，测试默认没有。
    fn meter(&self) -> Option<&crate::concurrency::meter::WorkMeter> {
        None
    }
    /// 当前处理的图片与相对路径；压缩会用它显示“正在处理哪张”。
    fn current(&self, _image_id: Option<i64>, _path: Option<&str>) {}
    /// 已提交批次的原图字节与节省字节。
    fn bytes(&self, _original: u64, _saved: u64) {}
    /// 运行中的累计跳过与失败张数。
    fn skipped_count(&self, _skipped: usize) {}
    /// 候选内跳过按原因细分（key 与估算视图的图例一致）；只有压缩会调用。
    fn skip_breakdown(&self, _buckets: &[(String, u64)]) {}
    fn failed_count(&self, _failed: usize) {}
}

/// 指定线程数建池；基准与测试用它比较不同线程数的吞吐。
///
/// 生产路径使用 `ImageBudget::pool`，线程数由全局图像预算决定。
pub(crate) fn encode_pool_with(threads: usize, context: &str) -> AppResult<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build()
        .map_err(|error| AppError::message(format!("无法创建{context}线程池：{error}")))
}

#[cfg(test)]
pub struct NoopReporter;

#[cfg(test)]
impl BatchReporter for NoopReporter {
    fn report(&self, _completed: usize, _total: usize) {}

    fn cancelled(&self) -> bool {
        false
    }
}

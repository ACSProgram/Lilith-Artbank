use crate::app::error::{AppError, AppResult};

pub mod capability;
pub mod collections;
pub mod content;
pub mod db;
pub mod edit;
pub mod folders;
pub mod history;
pub mod import;
pub mod manifest;
pub mod metadata;
pub mod optimize;
pub mod optimize_estimate;
pub mod paths;
pub mod query;
pub mod schema;
pub mod service;
pub mod similarity;
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
}

/// 压缩与估算共用的编码线程上限：两者都是整图编码，再多的线程在单块盘上也不会更快。
const ENCODE_MAX_THREADS: usize = 8;

/// 建一个整图编码线程池；`context` 只用于错误文案（“压缩” / “估算”）。
///
/// 独立线程池而不是全局 rayon 池：任务结束后线程立刻回收，不影响其他批量任务。
pub(crate) fn encode_pool(context: &str) -> AppResult<rayon::ThreadPool> {
    let threads = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, ENCODE_MAX_THREADS);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
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

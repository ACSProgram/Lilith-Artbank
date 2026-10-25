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
pub mod paths;
pub mod query;
pub mod schema;
pub mod service;
pub mod similarity;
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

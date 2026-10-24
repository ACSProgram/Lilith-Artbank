pub mod collections;
pub mod content;
pub mod db;
pub mod edit;
pub mod folders;
pub mod import;
pub mod manifest;
pub mod metadata;
pub mod paths;
pub mod query;
pub mod schema;
pub mod service;
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

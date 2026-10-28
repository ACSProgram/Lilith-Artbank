//! 进程内并发运行层：资源准入、路径锁、图像预算与进度计量。
//!
//! 这些类型不依赖 `app` 之外的状态机，`library` / `ai` / `mcp` 都可以直接使用，
//! 避免把“读图时短暂拿文件夹读锁”这类能力绑在 Tauri 命令层。

pub mod budget;
pub mod meter;
pub mod resources;

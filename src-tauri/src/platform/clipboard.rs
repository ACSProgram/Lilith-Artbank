use crate::app::error::{AppError, AppResult};

/// 写入系统剪贴板文本；前端不直接读图库目录，路径拼装留在 Rust。
pub fn copy_text(text: &str) -> AppResult<()> {
    if text.is_empty() {
        return Err(AppError::message("没有可复制的内容"));
    }
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| AppError::message(format!("无法打开剪贴板：{error}")))?;
    clipboard
        .set_text(text.to_owned())
        .map_err(|error| AppError::message(format!("无法写入剪贴板：{error}")))
}

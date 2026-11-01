use crate::app::error::{AppError, AppResult};

fn open() -> AppResult<arboard::Clipboard> {
    arboard::Clipboard::new().map_err(|error| AppError::message(format!("无法打开剪贴板：{error}")))
}

/// 写入系统剪贴板文本；前端不直接读图库目录，路径拼装留在 Rust。
pub fn copy_text(text: &str) -> AppResult<()> {
    if text.is_empty() {
        return Err(AppError::message("没有可复制的内容"));
    }
    open()?
        .set_text(text.to_owned())
        .map_err(|error| AppError::message(format!("无法写入剪贴板：{error}")))
}

/// 以文件引用（`CF_HDROP`）写入剪贴板；任意张数都不解码图片。
#[cfg(windows)]
pub fn copy_file_list(paths: &[String]) -> AppResult<()> {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::{GlobalFree, POINT},
        System::{
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::CF_HDROP,
        },
        UI::Shell::DROPFILES,
    };

    if paths.is_empty() {
        return Err(AppError::message("没有可复制的图片"));
    }

    // 每条路径以 UTF-16 NUL 结尾，整个列表再补一个 NUL；GMEM_ZEROINIT 负责补位。
    let entries: Vec<Vec<u16>> = paths
        .iter()
        .map(|path| path.encode_utf16().chain(std::iter::once(0)).collect())
        .collect();
    let header = std::mem::size_of::<DROPFILES>();
    let payload: usize = entries.iter().map(|entry| entry.len() * 2).sum();
    let size = header + payload + 2;

    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size) };
    if handle.is_null() {
        return Err(AppError::message("无法分配剪贴板内存"));
    }
    let memory = unsafe { GlobalLock(handle) };
    if memory.is_null() {
        unsafe { GlobalFree(handle) };
        return Err(AppError::message("无法锁定剪贴板内存"));
    }
    unsafe {
        ptr::write_unaligned(
            memory.cast::<DROPFILES>(),
            DROPFILES {
                pFiles: header as u32,
                pt: POINT { x: 0, y: 0 },
                fNC: 0,
                fWide: 1,
            },
        );
        let mut cursor = memory.cast::<u8>().add(header);
        for entry in &entries {
            let bytes = std::slice::from_raw_parts(entry.as_ptr().cast::<u8>(), entry.len() * 2);
            ptr::copy_nonoverlapping(bytes.as_ptr(), cursor, bytes.len());
            cursor = cursor.add(bytes.len());
        }
        GlobalUnlock(handle);
    }

    // 剪贴板可能被其它进程短暂占用，重试几次与 arboard 的打开策略一致。
    let opened = (0..5).any(|attempt| {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        (unsafe { OpenClipboard(ptr::null_mut()) }) != 0
    });
    if !opened {
        unsafe { GlobalFree(handle) };
        return Err(AppError::message(format!(
            "无法打开剪贴板：{}",
            std::io::Error::last_os_error()
        )));
    }

    let result = unsafe {
        if EmptyClipboard() == 0 {
            Err(format!(
                "无法清空剪贴板：{}",
                std::io::Error::last_os_error()
            ))
        } else if SetClipboardData(u32::from(CF_HDROP), handle).is_null() {
            Err(format!(
                "无法写入文件列表：{}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(())
        }
    };
    unsafe { CloseClipboard() };

    match result {
        Ok(()) => Ok(()),
        Err(message) => {
            unsafe { GlobalFree(handle) };
            Err(AppError::message(message))
        }
    }
}

#[cfg(not(windows))]
pub fn copy_file_list(_paths: &[String]) -> AppResult<()> {
    Err(AppError::message("复制图片仅支持 Windows"))
}

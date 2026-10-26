//! 断电安全的文件发布原语：持久化 rename 与目录 flush。
//!
//! 发布顺序固定为「文件数据 fsync → 持久化 rename → 目录 flush → 提交数据库」。
//! 只有前四步都返回成功，调用方才允许删旧图；否则断电后可能出现行指向不存在
//! 的文件，或旧图已删而新行未提交。

use std::path::Path;

use crate::app::error::{AppError, AppResult};

/// 把 `from` 改名到 `to`，并等待这次改名本身落盘。
///
/// Windows 用 `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`：
/// 调用返回时目录项变更已经写下去，这是本机断电保护的主要保证。其他平台退回
/// 标准 `rename`，由调用方随后 `flush_directory` 补上目录 fsync。
pub fn rename_durable(from: &Path, to: &Path) -> AppResult<()> {
    imp::rename(from, to).map_err(|error| {
        AppError::message(format!(
            "发布文件失败：{} → {}：{error}",
            from.display(),
            to.display()
        ))
    })
}

/// 让目录项变更（新建、改名、删除）落盘；同一批每个目标目录调用一次即可。
///
/// Windows 上 `FlushFileBuffers` 对目录句柄的支持取决于文件系统与系统版本：
/// 返回「不支持」时不算失败（rename 已经用 write-through 保证），只记调试日志；
/// 其他错误照常上报。非 Windows 平台打开目录并 `fsync`。
pub fn flush_directory(path: &Path) -> AppResult<()> {
    match imp::flush_dir(path) {
        Ok(()) => Ok(()),
        Err(FlushError::Unsupported(detail)) => {
            log::debug!("目录 flush 不受支持，依赖 write-through rename：{}", detail);
            Ok(())
        }
        Err(FlushError::Failed(detail)) => Err(AppError::message(format!(
            "刷新目录失败 {}：{detail}",
            path.display()
        ))),
    }
}

#[derive(Debug)]
enum FlushError {
    /// 当前系统/文件系统不提供目录 flush；rename 的 write-through 仍然有效。
    Unsupported(String),
    Failed(String),
}

#[cfg(windows)]
mod imp {
    use std::{io, os::windows::ffi::OsStrExt, path::Path, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FlushFileBuffers, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            MoveFileExW, OPEN_EXISTING,
        },
    };

    use super::FlushError;

    /// `FlushFileBuffers` 对目录句柄报这些错误时视为“系统不支持”，不算失败。
    const UNSUPPORTED_CODES: [i32; 5] = [1, 5, 6, 50, 87];

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    pub(super) fn rename(from: &Path, to: &Path) -> Result<(), String> {
        let from = wide(from);
        let to = wide(to);
        // SAFETY: 两个指针都指向以 NUL 结尾的 UTF-16 缓冲，调用期间有效。
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    pub(super) fn flush_dir(path: &Path) -> Result<(), FlushError> {
        let path = wide(path);
        // SAFETY: 路径缓冲以 NUL 结尾；目录句柄必须带 FILE_FLAG_BACKUP_SEMANTICS。
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let error = io::Error::last_os_error();
            return Err(classify(error));
        }
        // SAFETY: handle 是刚打开的有效句柄，关闭前不再使用。
        let ok = unsafe { FlushFileBuffers(handle) };
        let error = (ok == 0).then(io::Error::last_os_error);
        // SAFETY: handle 来自 CreateFileW，且只关闭一次。
        unsafe { CloseHandle(handle) };
        match error {
            Some(error) => Err(classify(error)),
            None => Ok(()),
        }
    }

    fn classify(error: io::Error) -> FlushError {
        match error.raw_os_error() {
            Some(code) if UNSUPPORTED_CODES.contains(&code) => {
                FlushError::Unsupported(error.to_string())
            }
            _ => FlushError::Failed(error.to_string()),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::{fs::File, path::Path};

    use super::FlushError;

    pub(super) fn rename(from: &Path, to: &Path) -> Result<(), String> {
        std::fs::rename(from, to).map_err(|error| error.to_string())
    }

    pub(super) fn flush_dir(path: &Path) -> Result<(), FlushError> {
        let directory = File::open(path).map_err(|error| FlushError::Failed(error.to_string()))?;
        directory
            .sync_all()
            .map_err(|error| FlushError::Failed(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn durable_rename_replaces_target_and_keeps_content() {
        let temp = tempdir().unwrap();
        let from = temp.path().join("new.tmp");
        let to = temp.path().join("target.bin");
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();

        rename_durable(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&to).unwrap(), b"new");
    }

    #[test]
    fn directory_flush_reports_platform_support() {
        let temp = tempdir().unwrap();
        let direct = imp::flush_dir(temp.path());
        match &direct {
            Ok(()) => println!("目录 flush：可用"),
            Err(FlushError::Unsupported(detail)) => {
                println!("目录 flush：系统不支持（{detail}），依赖 write-through rename")
            }
            Err(FlushError::Failed(detail)) => panic!("目录 flush 失败：{detail}"),
        }
        // 对外接口在“不支持”时按成功处理，其他错误照常上报。
        assert!(flush_directory(temp.path()).is_ok(), "{direct:?}");
    }
}

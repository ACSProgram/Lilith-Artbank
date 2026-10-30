use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use crate::{
    app::error::{AppError, AppResult},
    library::content,
    platform::fs as durable,
};

use super::READ_BUFFER;

pub(super) fn copy_and_hash(source: &Path, destination: &Path) -> AppResult<(String, u64)> {
    let mut input = BufReader::with_capacity(READ_BUFFER, File::open(source)?);
    let mut output = BufWriter::with_capacity(READ_BUFFER, File::create(destination)?);
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; READ_BUFFER];
    let mut size = 0_u64;

    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
        size += read as u64;
    }
    output.flush()?;
    output.get_ref().sync_all()?;

    if size == 0 {
        return Err(AppError::message("文件为空"));
    }
    if size > i64::MAX as u64 {
        return Err(AppError::message("文件大小超出支持范围"));
    }

    Ok((hasher.finalize().to_hex().to_string(), size))
}

/// 把待发布文件发布为目标内容文件。
///
/// `temp` 是导入临时目录里的文件：原图副本，或压缩结果。目标已存在时不能直接
/// 当成“内容重复”——上次 rename 后、入库前崩溃会留下没有任何行引用的孤儿，文件
/// 可能被截断。先按 `expected_hash` 核对内容：一致就接管，不一致就删掉重发。
///
/// 返回 `true` 表示这次确实由本批写入目标文件；`false` 表示目标已有可用文件，
/// 调用方在入库失败时不能删它。
pub(super) fn publish(temp: &Path, destination: &Path, expected_hash: &str) -> AppResult<bool> {
    if destination.is_file() {
        if content::hash_file(destination)? == expected_hash {
            log::warn!("导入目标已存在且内容一致，接管：{}", destination.display());
            let _ = std::fs::remove_file(temp);
            return Ok(false);
        }
        log::warn!(
            "导入目标内容与预期不符，删除后重新发布：{}",
            destination.display()
        );
        std::fs::remove_file(destination)?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    durable::rename_durable(temp, destination)?;
    if let Some(parent) = destination.parent() {
        durable::flush_directory(parent)?;
    }
    Ok(true)
}

/// 压缩结果先落到导入临时目录，再走与原图相同的发布路径。
pub(super) fn write_encoded(bytes: &[u8], staged: &Path) -> AppResult<PathBuf> {
    let path = staged.with_extension("optimized");
    let write = || -> std::io::Result<()> {
        let mut file = BufWriter::with_capacity(READ_BUFFER, File::create(&path)?);
        file.write_all(bytes)?;
        file.flush()?;
        file.get_ref().sync_all()
    };
    if let Err(error) = write() {
        let _ = std::fs::remove_file(&path);
        return Err(AppError::message(format!("写入压缩结果失败：{error}")));
    }
    Ok(path)
}

/// 原图扩展名取自解码后的格式，不跟源文件名：否则把 PNG 改名成 `.jpg` 入库后，
/// 文件扩展名会和 `images.format` 对不上。
pub(super) fn content_extension(format: &str) -> &str {
    match format {
        "jpeg" => "jpg",
        other => other,
    }
}

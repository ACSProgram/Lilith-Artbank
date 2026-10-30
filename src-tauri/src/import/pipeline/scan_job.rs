use std::{path::Path, sync::atomic::Ordering};

use uuid::Uuid;

use crate::{
    app::error::AppResult,
    concurrency::budget::estimate_working_bytes,
    imageops::{
        colors,
        optimize::{self, LadderOutcome, OptimizeFormat, OptimizeOptions, SourceInfo},
        phash, thumbnail,
    },
    import::{FolderMode, decode},
    library::{
        import as repository,
        paths::{join_folder, normalize_folder},
        service::now_ms,
    },
};

use super::{
    DuplicateOutcome, Outcome, PhashIndex, Prepared, Staged, WorkerContext,
    publish::{content_extension, copy_and_hash, publish, write_encoded},
};

pub(super) fn process_one(path: &Path, relative_folder: &str, context: &WorkerContext) {
    if context.job.should_cancel() {
        return;
    }
    #[cfg(test)]
    if context
        .cancel_after
        .is_some_and(|limit| context.job.processed() >= limit)
    {
        context.job.request_cancel();
    }

    if let Some(name) = path.file_name() {
        context.job.set_current(name.to_string_lossy());
    }

    let result = {
        let Ok(_permit) = context.images.acquire(&|| context.job.should_cancel()) else {
            return;
        };
        process_file(path, relative_folder, context)
    };
    match result {
        Ok(Some(outcome)) => {
            context.job.inc_processed();
            if context.sender.send(outcome).is_err() {
                log::warn!("导入写通道已关闭，结果被丢弃：{}", path.display());
            }
        }
        Ok(None) => {}
        Err(error) => {
            context
                .job
                .add_failure(path.to_string_lossy(), error.to_string());
            context.job.inc_processed();
            context.job.inc_failed();
        }
    }
}

fn process_file(
    path: &Path,
    relative_folder: &str,
    context: &WorkerContext,
) -> AppResult<Option<Outcome>> {
    let staged = stage_file(path, &context.temp_dir)?;
    let result = process_staged(path, relative_folder, &staged, context);
    let _ = std::fs::remove_file(&staged.path);
    result
}

fn process_staged(
    source_path: &Path,
    relative_folder: &str,
    staged: &Staged,
    context: &WorkerContext,
) -> AppResult<Option<Outcome>> {
    if context.job.should_cancel() {
        return Ok(None);
    }

    // 库内已有内容优先归因为“精确重复”；只有本次导入尚未入库的哈希才进入批内占位。
    // 图库导入不看临时区的行：临时区只是“先看一眼”的暂存，同内容的图要真正入库。
    let existing = context.database.with_reader(|connection| {
        repository::find_by_hash_state(connection, &staged.hash, context.dedup_includes_temp)
    })?;
    if let Some((existing_id, trashed)) = existing {
        return Ok(Some(Outcome::Duplicate(DuplicateOutcome {
            hash: staged.hash.clone(),
            source_path: source_path.to_path_buf(),
            in_batch: false,
            existing_id: Some(existing_id),
            trashed,
            kept_path: None,
        })));
    }

    {
        let mut inflight = context.inflight.lock();
        if let Some(kept_path) = inflight.get(&staged.hash) {
            return Ok(Some(Outcome::Duplicate(DuplicateOutcome {
                hash: staged.hash.clone(),
                source_path: source_path.to_path_buf(),
                in_batch: true,
                existing_id: None,
                trashed: false,
                kept_path: Some(kept_path.clone()),
            })));
        }
        inflight.insert(staged.hash.clone(), source_path.to_path_buf());
    }

    let result = build_prepared(source_path, relative_folder, staged, context);
    if !matches!(result, Ok(Some(_))) {
        context.inflight.lock().remove(&staged.hash);
    }
    result
}

fn build_prepared(
    source_path: &Path,
    relative_folder: &str,
    staged: &Staged,
    context: &WorkerContext,
) -> AppResult<Option<Outcome>> {
    if context.job.should_cancel() {
        return Ok(None);
    }

    // 先按文件头估算峰值，再申请解码内存额度；大图会排队，小图照常并行。
    let estimate = match decode::probe_dimensions(&staged.path) {
        Ok((width, height)) => {
            estimate_working_bytes(width, height, staged.size, context.optimize.is_some())
        }
        Err(error) => {
            // 少数格式要读到完整数据才知道尺寸；按保守额度放行，解码器仍会做最终限制。
            log::debug!(
                "无法预读尺寸，按保守额度估算：{}：{error}",
                staged.path.display()
            );
            if context.optimize.is_some() {
                256 * 1024 * 1024
            } else {
                64 * 1024 * 1024
            }
        }
    };
    context.max_estimate.fetch_max(estimate, Ordering::Relaxed);
    let Ok(memory) = context
        .memory
        .acquire(estimate, &|| context.job.should_cancel())
    else {
        return Ok(None);
    };

    let decoded = decode::decode(&staged.path)?;
    let decoded_format = decoded.format;
    let width = decoded.width;
    let height = decoded.height;
    let source_has_alpha = decoded.has_alpha;
    let thumb = thumbnail::encode(&decoded.image)?;
    let hash = phash::compute(&decoded.image);
    let (colors4, colors8) = colors::dominant(&decoded.image);
    let similar = find_similar(
        &context.index.read(),
        &phash::words(&hash),
        context.threshold,
    );

    if context.job.should_cancel() {
        return Ok(None);
    }

    let folder = match context.folder_mode {
        FolderMode::Preserve if !relative_folder.is_empty() => {
            normalize_folder(&join_folder(&context.folder, relative_folder))?
        }
        _ => context.folder.clone(),
    };

    // 缩略图、pHash、主色都来自同一张解码图，压缩前后字节不同但像素相同，
    // 所以这三样不需要因为压缩而重算。任何一步不满足判据都保留原图。
    let mut content = staged.path.clone();
    let mut content_hash = staged.hash.clone();
    let mut file_ext = content_extension(decoded_format).to_owned();
    let mut file_size = staged.size;
    let mut format = decoded_format.to_owned();
    let mut has_alpha = source_has_alpha;
    let mut optimized_at = 0_i64;
    let mut optimized_savings = None;
    if let Some(options) = &context.optimize {
        match optimize_for_import(&staged.path, decoded, options) {
            Ok(Some(candidate)) => {
                let bytes = candidate.bytes;
                content = write_encoded(&bytes, &staged.path)?;
                content_hash = blake3::hash(&bytes).to_hex().to_string();
                file_ext = options.output_format.extension().to_owned();
                file_size = bytes.len() as u64;
                format = options.output_format.name().to_owned();
                // JPEG 装不下透明通道；能在这一步拿到结果的带 alpha 图只会是 WebP。
                has_alpha = options.output_format != OptimizeFormat::Jpeg && source_has_alpha;
                optimized_at = now_ms();
                optimized_savings = Some(staged.size.saturating_sub(file_size));
            }
            // 判据不满足：保留原图，导入照常成功。
            Ok(None) => {}
            Err(error) => {
                // 编码器异常与「压不动」是两回事，单独计数并在完成页说出来。
                context.job.inc_optimize_failed();
                log::warn!(
                    "导入时压缩失败，保留原图：{}：{error}",
                    source_path.display()
                );
            }
        }
    } else {
        drop(decoded);
    }
    // 特征与压缩都已结束：先还解码额度，再发布文件，写线程积压不再占解码内存。
    drop(memory);

    let file_name = format!("{content_hash}.{file_ext}");
    let destination = context.paths.content_file(&folder, &file_name);
    let published = publish(&content, &destination, &content_hash)?;

    Ok(Some(Outcome::Prepared(Prepared {
        source_path: source_path.to_path_buf(),
        destination,
        folder,
        published,
        content_hash,
        file_ext,
        file_size,
        width,
        height,
        format,
        has_alpha,
        phash: hash,
        colors4,
        colors8,
        thumb,
        similar,
        optimized_at,
        optimized_savings,
    })))
}

/// 导入时的压缩：完整 PSNR 阶梯与批量压缩共用同一份实现。
///
/// 返回 `Ok(None)` 表示按判据保留原图（动画/多页、非 sRGB、超大、16 位、省不到 5%）；
/// 返回 `Err` 表示编码器真的出错，调用方要单独计数，不能当「压不动」静默吞掉。
fn optimize_for_import(
    staged: &Path,
    decoded: decode::DecodedImage,
    options: &OptimizeOptions,
) -> AppResult<Option<optimize::EncodedCandidate>> {
    // ICC 判据与 JPEG→JPEG 的 EXIF 搬运都要原始字节，所以这里再读一遍临时副本。
    let source_bytes = std::fs::read(staged)?;
    let source_size = source_bytes.len() as u64;
    // 与批量压缩同序：动画 / 多帧 / 多页源先拦下，否则会被压成单帧且不可逆。
    if optimize::multiframe_guard(&source_bytes, decoded.format).is_err() {
        return Ok(None);
    }
    if optimize::color_profile_guard(&source_bytes, decoded.format, options).is_err() {
        return Ok(None);
    }
    let source = SourceInfo {
        format: decoded.format.to_owned(),
        file_size: source_size as i64,
        width: decoded.width,
        height: decoded.height,
        has_alpha: decoded.has_alpha,
        optimized_at: 0,
        bit_depth: optimize::bit_depth(&decoded.image),
    };
    let Ok(plan) = optimize::plan(&source, options) else {
        return Ok(None);
    };
    // 只有 JPEG → JPEG 会搬运源文件的 EXIF；其他组合由编码器写新头。
    let source_jpeg = (options.output_format == OptimizeFormat::Jpeg
        && matches!(decoded.format, "jpeg" | "jpg"))
    .then_some(source_bytes.as_slice());
    // 不搬 EXIF 时源文件字节到这里就没用了，先释放，别和编码峰值叠在一起。
    let outcome = match source_jpeg {
        Some(jpeg) => {
            optimize::encode_ladder_owned(decoded.image, &plan, options, source_size, Some(jpeg))?
        }
        None => {
            drop(source_bytes);
            optimize::encode_ladder_owned(decoded.image, &plan, options, source_size, None)?
        }
    };
    match outcome {
        LadderOutcome::Encoded(candidate) => Ok(Some(candidate)),
        LadderOutcome::Skipped(_) => Ok(None),
    }
}

fn find_similar(index: &PhashIndex, hash: &[u64; 4], threshold: u8) -> Option<(i64, u32)> {
    let threshold = u32::from(threshold);
    let mut best: Option<(i64, u32)> = None;
    for (id, candidate) in index {
        let distance = phash::distance_words(hash, candidate);
        if distance <= threshold {
            if best.is_none_or(|(_, current)| distance < current) {
                best = Some((*id, distance));
            }
            if distance == 0 {
                break;
            }
        }
    }
    best
}

fn stage_file(source: &Path, temp_dir: &Path) -> AppResult<Staged> {
    // 临时文件保留原扩展名：TGA 这类格式的魔数很弱，解码器需要扩展名兜底。
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "img".to_owned());
    let temp_path = temp_dir.join(format!("{}.{}", Uuid::new_v4(), extension));
    let result = copy_and_hash(source, &temp_path);
    match result {
        Ok((hash, size)) => Ok(Staged {
            path: temp_path,
            hash,
            size,
        }),
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

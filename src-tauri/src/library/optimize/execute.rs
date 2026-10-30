use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    time::Instant,
};

use parking_lot::Mutex;
use rayon::prelude::*;

use crate::{
    app::error::{AppError, AppResult},
    concurrency::budget::{BudgetError, ImageBudget, ImageMemoryBudget, estimate_working_bytes},
    imageops::{
        colors,
        optimize::{self, LadderOutcome, OptimizeFormat, OptimizeOptions, SkipReason, SourceInfo},
        phash, thumbnail,
    },
    import::decode,
    library::{
        BatchReporter,
        calibration::{OptimizeCalibration, RunSample},
        content,
        db::LibraryDb,
        metadata,
        paths::LibraryPaths,
        thumbs::ThumbPack,
    },
};

use super::{
    BATCH_SIZE, OptimizeFailure, OptimizeReport,
    candidates::{CHUNK, CandidateRow},
    commit::{commit_batch, delete_replaced},
};

const MAX_BATCH_BYTES: u64 = 256 * 1024 * 1024;
/// 批内每编码这么多张更新一次 message；256 张一批时分 16 次，5 张/秒也约 3 秒一跳。
const MESSAGE_STEP: usize = 16;
/// 连续失败达到这个数就停止，避免磁盘或解码器故障时把整库刷成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;
const MAX_REPORTED_FAILURES: usize = 50;

/// 临时文件句柄：作用域结束时自动删除，成功发布后 `disarm` 交给目标文件。
pub(super) struct TempPath(Option<PathBuf>);

impl Drop for TempPath {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl TempPath {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    pub(super) fn path(&self) -> &Path {
        self.0.as_deref().expect("临时文件路径在 disarm 前始终可用")
    }

    pub(super) fn disarm(&mut self) {
        self.0 = None;
    }
}

pub(super) struct EncodedItem {
    pub(super) row_id: i64,
    pub(super) folder_path: String,
    pub(super) old_hash: String,
    pub(super) old_path: PathBuf,
    pub(super) old_size: u64,
    pub(super) new_hash: String,
    pub(super) new_ext: &'static str,
    pub(super) new_format: &'static str,
    pub(super) new_size: u64,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) has_alpha: bool,
    pub(super) temp: TempPath,
    pub(super) thumb: Option<thumbnail::EncodedThumbnail>,
    pub(super) phash: Option<[u8; 32]>,
    pub(super) colors: Option<([u8; 16], [u8; 32])>,
    encode_ms: u64,
    hash_ms: u64,
    recalc_ms: u64,
    write_ms: u64,
    fsync_ms: u64,
}

/// 单张编码结果：要么可以发布，要么按候选内跳过处理。
enum EncodeOutcome {
    Encoded(Box<EncodedItem>),
    Skipped(SkipReason),
    /// 原图缺失或读不出来；单独计数，不累加连续失败。
    SourceMissing(String),
}

/// 执行入口；`candidates` 由调用方按同一范围与参数收集，`operation_id` 是 live 记录。
#[allow(clippy::too_many_arguments)]
pub fn optimize(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    candidates: Vec<CandidateRow>,
    options: &OptimizeOptions,
    operation_id: i64,
    reporter: &dyn BatchReporter,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
) -> AppResult<OptimizeReport> {
    optimize_with_batch_size(
        database,
        root,
        thumbs,
        candidates,
        options,
        operation_id,
        reporter,
        BATCH_SIZE,
        images,
        memory,
    )
}

struct EncodedBatch {
    index: usize,
    items: Vec<EncodedItem>,
}

fn batch_slices(jobs: &[CandidateRow], max_count: usize, max_bytes: u64) -> Vec<&[CandidateRow]> {
    let mut slices = Vec::new();
    let mut start = 0;
    while start < jobs.len() {
        let mut end = start;
        let mut bytes = 0_u64;
        while end < jobs.len() && end - start < max_count {
            let size = jobs[end].file_size.max(1) as u64;
            if end > start && bytes.saturating_add(size) > max_bytes {
                break;
            }
            bytes = bytes.saturating_add(size);
            end += 1;
        }
        slices.push(&jobs[start..end]);
        start = end;
    }
    slices
}

/// 全部跳过张数（含执行前排除的图片）；原图缺失也计入，用于最终报告。
pub(super) fn skipped_count(report: &OptimizeReport) -> usize {
    (report.prefilter_skipped
        + report.candidate_skipped
        + report.duplicate_skipped
        + report.source_missing) as usize
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn optimize_with_batch_size(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    candidates: Vec<CandidateRow>,
    options: &OptimizeOptions,
    operation_id: i64,
    reporter: &dyn BatchReporter,
    batch_size: usize,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
) -> AppResult<OptimizeReport> {
    options.validate()?;
    let started = Instant::now();
    let paths = LibraryPaths::new(root);
    std::fs::create_dir_all(&paths.temp)?;

    let mut report = OptimizeReport::default();
    let mut jobs: Vec<CandidateRow> = Vec::with_capacity(candidates.len());
    for row in candidates {
        if optimize::prefilter(&row.source_info(), options).is_err() {
            report.prefilter_skipped += 1;
            continue;
        }
        jobs.push(row);
    }
    // 先压 bpp 高的图片：同样耗时时收益更大，与估算里的收益曲线顺序一致。
    jobs.sort_by(|left, right| right.bpp().total_cmp(&left.bpp()));

    let total = jobs.len();
    // 进度按张计，且只在候选池里算：
    // - 按像素计权会被 bpp 降序抵消——排在前面的都是「像素少、细节高」的小图，
    //   前几千张推不动进度条；按张计与 embedding 同口径，速度与 ETA 也一起对齐。
    // - 执行前被排除的图片（已处理过、bpp 太低、超出 WebP 尺寸）不需要任何工作，
    //   算进分母会让进度条开局就六成；它们在估算视图的「未纳入候选」里单独列出。
    // 计量器只用来算滚动吞吐与 ETA：百分比一律由「已结算张数 / 候选张数」派生，
    // 否则任务胶囊、进度条与环图会各自带着不同的权重系数。
    if let Some(meter) = reporter.meter() {
        meter.set_total_units(total.max(1) as u64);
    }
    reporter.report(0, total);
    reporter.skipped_count(0);

    let report = Arc::new(Mutex::new(report));
    let stopped = Arc::new(AtomicBool::new(false));
    let error_slot: Arc<Mutex<Option<AppError>>> = Arc::new(Mutex::new(None));
    let batches_committed = Arc::new(AtomicU64::new(0));
    let pool = Arc::new(images.pool("压缩")?);
    let (sender, receiver) = mpsc::sync_channel::<EncodedBatch>(1);

    std::thread::scope(|scope| {
        let commit_report = report.clone();
        let commit_error = error_slot.clone();
        let commit_batches = batches_committed.clone();
        let commit_pool = pool.clone();
        let commit_paths = &paths;
        let committer = scope.spawn(move || {
            while let Ok(batch) = receiver.recv() {
                if batch.items.is_empty() {
                    continue;
                }
                reporter.message(&format!("正在写入第 {} 批", batch.index + 1));
                // 一批开始写入时不再对应某一张图，避免界面停在上一张。
                reporter.current(None, None);
                let outcome =
                    match commit_batch(database, commit_paths, thumbs, operation_id, batch.items) {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            *commit_error.lock() = Some(error);
                            return;
                        }
                    };
                let published = outcome.published.len();
                let (processed_before, failed_now, skipped_now, buckets, original, saved) = {
                    let mut report = commit_report.lock();
                    let before = report.processed as usize;
                    report.publish_ms += outcome.write_ms;
                    report.commit_ms += outcome.commit_ms;
                    report.duplicate_skipped += outcome.duplicates;
                    report.processed += published as u64;
                    for item in &outcome.published {
                        report.old_bytes += item.old_size;
                        report.new_bytes += item.new_size;
                    }
                    (
                        before,
                        report.failed as usize,
                        report.progress_skipped(),
                        report.skip_buckets(),
                        report.old_bytes,
                        report.old_bytes.saturating_sub(report.new_bytes),
                    )
                };
                reporter.bytes(original, saved);
                reporter.failed_count(failed_now);
                reporter.skipped_count(skipped_now);
                reporter.skip_breakdown(&buckets);
                commit_batches.fetch_add(1, Ordering::Relaxed);
                // 已完成只统计真正入库的图片；批次内逐张上报，进度不按批跳跃。
                for step in 1..=published {
                    reporter.report(processed_before + step, total);
                }
                // 提交已持久化，才允许删旧图；删除不持数据库写锁，失败只记日志。
                let mut report = commit_report.lock();
                delete_replaced(
                    &commit_pool,
                    root,
                    commit_paths,
                    &outcome.published,
                    &mut report,
                );
            }
        });

        let slices = batch_slices(&jobs, batch_size.max(1), MAX_BATCH_BYTES);
        let mut consecutive_failures = 0_usize;
        for (batch_index, batch) in slices.into_iter().enumerate() {
            if reporter.cancelled() || stopped.load(Ordering::Relaxed) {
                break;
            }
            reporter.message(&format!(
                "正在编码… 第 {} 批 0/{}",
                batch_index + 1,
                batch.len()
            ));
            // 已提交张数只在批次结束时跳一次；批内计数走 message，让界面持续有变化。
            let encoded = AtomicUsize::new(0);
            let results = pool.install(|| {
                batch
                    .par_iter()
                    .map(|row| {
                        if reporter.cancelled() {
                            return Ok(None);
                        }
                        let permit = match images.acquire(&|| reporter.cancelled()) {
                            Ok(permit) => permit,
                            Err(BudgetError::Cancelled) => return Ok(None),
                        };
                        // 线程许可在前、字节额度在后，与导入、估算和 AI 解码的顺序一致。
                        let estimate = estimate_working_bytes(
                            row.width,
                            row.height,
                            row.file_size.max(0) as u64,
                            true,
                        );
                        let memory_permit = match memory.acquire(estimate, &|| reporter.cancelled())
                        {
                            Ok(permit) => permit,
                            Err(BudgetError::Cancelled) => return Ok(None),
                        };
                        let relative = content::relative_display_path(
                            &row.folder_path,
                            &row.content_hash,
                            &row.file_ext,
                        );
                        reporter.current(Some(row.id), Some(&relative));
                        let result = encode_one(&paths, row, options);
                        drop(memory_permit);
                        drop(permit);
                        if let Some(meter) = reporter.meter() {
                            meter.advance_units(1);
                        }
                        let step = encoded.fetch_add(1, Ordering::Relaxed) + 1;
                        if step.is_multiple_of(MESSAGE_STEP) || step == batch.len() {
                            reporter.message(&format!(
                                "正在编码… 第 {} 批 {step}/{}",
                                batch_index + 1,
                                batch.len()
                            ));
                        }
                        result.map(Some)
                    })
                    .collect::<Vec<AppResult<Option<EncodeOutcome>>>>()
            });

            let mut items = Vec::new();
            let (failed_now, skipped_now) = {
                let mut report = report.lock();
                for (row, result) in batch.iter().zip(results) {
                    match result {
                        Ok(Some(EncodeOutcome::Encoded(item))) => {
                            report.encode_ms += item.encode_ms;
                            report.hash_ms += item.hash_ms;
                            report.recalc_ms += item.recalc_ms;
                            report.write_ms += item.write_ms;
                            report.fsync_ms += item.fsync_ms;
                            items.push(*item);
                            consecutive_failures = 0;
                        }
                        Ok(Some(EncodeOutcome::Skipped(reason))) => {
                            log::debug!("跳过 image={}：{reason:?}", row.id);
                            report.record_skip(reason);
                            consecutive_failures = 0;
                        }
                        Ok(Some(EncodeOutcome::SourceMissing(reason))) => {
                            log::debug!("原图缺失 image={}：{reason}", row.id);
                            report.source_missing += 1;
                            if report.failures.len() < MAX_REPORTED_FAILURES {
                                report.failures.push(OptimizeFailure {
                                    image_id: row.id,
                                    path: display_path(&paths, row),
                                    reason,
                                });
                            }
                        }
                        Ok(None) => {
                            consecutive_failures = 0;
                        }
                        Err(error) => {
                            report.failed += 1;
                            consecutive_failures += 1;
                            if report.failures.len() < MAX_REPORTED_FAILURES {
                                report.failures.push(OptimizeFailure {
                                    image_id: row.id,
                                    path: display_path(&paths, row),
                                    reason: error.to_string(),
                                });
                            }
                            if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                                report.stop_reason =
                                    Some("连续多张图片处理失败，任务已停止".to_owned());
                                stopped.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
                (report.failed as usize, report.progress_skipped())
            };
            reporter.failed_count(failed_now);
            reporter.skipped_count(skipped_now);

            if !items.is_empty()
                && sender
                    .send(EncodedBatch {
                        index: batch_index,
                        items,
                    })
                    .is_err()
            {
                break;
            }
            if stopped.load(Ordering::Relaxed) || reporter.cancelled() {
                break;
            }
        }
        drop(sender);
        let _ = committer.join();
    });

    let mut report = std::mem::take(&mut *report.lock());
    if let Some(error) = error_slot.lock().take() {
        return Err(error);
    }
    if reporter.cancelled() {
        report.cancelled = true;
    }
    report.skipped = skipped_count(&report) as u64;
    report.savings_bytes = report.old_bytes.saturating_sub(report.new_bytes);
    // 收尾再对一次，保证取消/失败时面板与最终报告一致。
    reporter.bytes(report.old_bytes, report.savings_bytes);
    reporter.failed_count(report.failed as usize);
    // 最终报告里的 skipped 含执行前被排除的图片，进度只认候选池内的跳过。
    reporter.skipped_count(report.progress_skipped());
    reporter.skip_breakdown(&report.skip_buckets());
    reporter.current(None, None);
    report.elapsed_ms = started.elapsed().as_millis() as u64;

    // 用本次实测覆盖本库校准值，供下次估算替换保守常数；样本太少时保留旧值。
    let sample = RunSample {
        processed: report.processed,
        output_bytes: report.new_bytes,
        batches: batches_committed.load(Ordering::Relaxed),
        write_secs: (report.write_ms + report.publish_ms) as f64 / 1000.0,
        fsync_secs: report.fsync_ms as f64 / 1000.0,
        commit_secs: report.commit_ms as f64 / 1000.0,
        delete_secs: report.delete_ms as f64 / 1000.0,
    };
    if let Some(calibration) = OptimizeCalibration::from_run(&sample)
        && let Err(error) = calibration.save(database)
    {
        log::warn!("写入压缩校准值失败：{error}");
    }

    Ok(report)
}

/// 失败清单里展示的路径；解析不出内容路径时退回「文件夹/哈希」。
fn display_path(paths: &LibraryPaths, row: &CandidateRow) -> String {
    metadata::content_path(paths, &row.folder_path, &row.content_hash, &row.file_ext)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| format!("{}/{}", row.folder_path, row.content_hash))
}

/// 单张编码：只写临时文件，不碰数据库与原图；未通过判据时返回跳过原因。
fn encode_one(
    paths: &LibraryPaths,
    row: &CandidateRow,
    options: &OptimizeOptions,
) -> AppResult<EncodeOutcome> {
    let source_path =
        metadata::content_path(paths, &row.folder_path, &row.content_hash, &row.file_ext)?;
    // 只读一次盘：色彩配置、动画判据、解码与 EXIF 搬运共用这份字节。
    let bytes = match std::fs::read(&source_path) {
        Ok(bytes) => bytes,
        // 原图被外部删掉或挪走：不当作解码/磁盘故障，否则 5 张缺失就会停掉整任务。
        Err(error) => {
            return Ok(EncodeOutcome::SourceMissing(format!(
                "原图缺失或无法读取：{error}"
            )));
        }
    };
    if optimize::multiframe_guard(&bytes, &row.format).is_err() {
        return Ok(EncodeOutcome::Skipped(SkipReason::AnimatedOrMultipage));
    }
    if optimize::color_profile_guard(&bytes, &row.format, options).is_err() {
        return Ok(EncodeOutcome::Skipped(SkipReason::ColorProfile));
    }

    let encode_started = Instant::now();
    let decoded = decode::decode_bytes(&bytes, Some(&row.file_ext))?;
    let source = SourceInfo {
        format: row.format.clone(),
        file_size: row.file_size,
        width: decoded.width,
        height: decoded.height,
        has_alpha: row.has_alpha || decoded.has_alpha,
        optimized_at: row.optimized_at,
        bit_depth: optimize::bit_depth(&decoded.image),
    };
    let plan = match optimize::plan(&source, options) {
        Ok(plan) => plan,
        Err(reason) => return Ok(EncodeOutcome::Skipped(reason)),
    };

    let prepared = optimize::prepare(decoded.image, options.max_long_edge);
    let source_jpeg = matches!(row.format.as_str(), "jpeg" | "jpg")
        .then_some(bytes.as_slice())
        .filter(|_| options.output_format == OptimizeFormat::Jpeg);
    let candidate = match optimize::encode_ladder(
        &prepared,
        &plan,
        options,
        bytes.len() as u64,
        source_jpeg,
    )? {
        LadderOutcome::Encoded(candidate) => candidate,
        LadderOutcome::Skipped(reason) => return Ok(EncodeOutcome::Skipped(reason)),
    };
    let encode_ms = encode_started.elapsed().as_millis() as u64;
    let hash_started = Instant::now();
    let new_hash = blake3::hash(&candidate.bytes).to_hex().to_string();
    let hash_ms = hash_started.elapsed().as_millis() as u64;
    // 重新编码出的字节和原文件完全一致：没有任何收益，按“省得太少”计。
    if new_hash == row.content_hash {
        return Ok(EncodeOutcome::Skipped(SkipReason::NotSmallEnough));
    }

    let (width, height) = (prepared.width(), prepared.height());
    let temp_path = paths.temp.join(format!(
        "optimize-{}-{}.{}",
        row.id,
        uuid::Uuid::new_v4(),
        options.output_format.extension()
    ));
    let temp = TempPath::new(temp_path);
    let write_started = Instant::now();
    let mut file = std::fs::File::create(temp.path())?;
    file.write_all(&candidate.bytes)?;
    let fsync_started = Instant::now();
    file.sync_all()?;
    let fsync_ms = fsync_started.elapsed().as_millis() as u64;
    let write_ms = write_started.elapsed().as_millis() as u64;
    drop(file);

    let recalc_started = Instant::now();
    let thumb = if options.regenerate_thumbnails {
        Some(thumbnail::encode(&prepared)?)
    } else {
        None
    };
    let phash = options.recompute_phash.then(|| phash::compute(&prepared));
    let colors = options
        .recompute_colors
        .then(|| colors::dominant(&prepared));
    let recalc_ms = recalc_started.elapsed().as_millis() as u64;

    Ok(EncodeOutcome::Encoded(Box::new(EncodedItem {
        row_id: row.id,
        folder_path: row.folder_path.clone(),
        old_hash: row.content_hash.clone(),
        old_path: source_path,
        old_size: bytes.len() as u64,
        new_hash,
        new_ext: options.output_format.extension(),
        new_format: options.output_format.name(),
        new_size: candidate.bytes.len() as u64,
        width,
        height,
        has_alpha: prepared.color().has_alpha(),
        temp,
        thumb,
        phash,
        colors,
        encode_ms,
        hash_ms,
        recalc_ms,
        write_ms,
        fsync_ms,
    })))
}

/// 删掉本批已发布却没有行引用的新文件；清理失败只记日志，不盖掉真正的错误。
pub(super) fn discard_written(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = std::fs::remove_file(path) {
            log::warn!("清理未入库的压缩结果失败 {}：{error}", path.display());
        }
    }
}

/// 本批新哈希已被哪些行引用；分块查询，避免一次塞进上千个参数。
pub(super) fn referenced_hashes(
    database: &LibraryDb,
    items: &[EncodedItem],
) -> AppResult<HashSet<String>> {
    let hashes: Vec<&str> = items.iter().map(|item| item.new_hash.as_str()).collect();
    let mut referenced = HashSet::new();
    for chunk in hashes.chunks(CHUNK) {
        let sql = format!(
            "SELECT content_hash FROM images WHERE content_hash IN ({})",
            crate::library::placeholders(chunk.len())
        );
        database.with_reader(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            referenced.extend(rows);
            Ok(())
        })?;
    }
    Ok(referenced)
}

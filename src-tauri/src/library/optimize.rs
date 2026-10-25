//! 图片空间优化的写路径：候选收集、凭证校验、分批编码发布与旧图清理。
//!
//! 每张图先编码到统一临时目录，达到判据后才发布为新的内容哈希文件；
//! 1000 张一个事务，事务内发布并更新行，提交后再删旧图，提交失败则删新图。
//! 压缩不可撤销：记录只在时间线上占位并说明原图已删除。

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use rayon::prelude::*;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    imageops::{
        colors,
        optimize::{self, LadderOutcome, OptimizeFormat, OptimizeOptions, SourceInfo},
        phash, thumbnail,
    },
    import::decode,
    library::{
        BatchReporter, db::LibraryDb, encode_pool, import as repository, metadata,
        optimize_estimate::OptimizeEstimate, paths::LibraryPaths, service::now_ms,
        thumbs::ThumbPack, viewer,
    },
};

/// 每批编码完成后在同一事务里发布，限制内存峰值与事务时长。
const BATCH_SIZE: usize = 1000;
/// 连续失败达到这个数就停止，避免磁盘或解码器故障时把整库刷成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;
const MAX_REPORTED_FAILURES: usize = 50;
const CHUNK: usize = 400;

#[derive(Debug, Clone)]
pub struct CandidateRow {
    pub id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub folder_id: i64,
    pub folder_path: String,
    pub file_size: i64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub has_alpha: bool,
    pub optimized_at: i64,
}

impl CandidateRow {
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    pub fn bpp(&self) -> f64 {
        let pixels = self.pixels();
        if pixels == 0 {
            return 0.0;
        }
        self.file_size.max(0) as f64 * 8.0 / pixels as f64
    }

    pub fn lossless_source(&self) -> bool {
        !matches!(self.format.as_str(), "jpeg" | "jpg" | "webp")
    }

    /// 只依赖数据库列的源信息；位深按 8 位占位，解码后再用真实值覆盖。
    pub fn source_info(&self) -> SourceInfo {
        SourceInfo {
            format: self.format.clone(),
            file_size: self.file_size,
            width: self.width,
            height: self.height,
            has_alpha: self.has_alpha,
            optimized_at: self.optimized_at,
            bit_depth: 8,
        }
    }
}

/// 按 id 分块读候选行；只取未删除的图片。
pub fn collect_candidates(database: &LibraryDb, ids: &[i64]) -> AppResult<Vec<CandidateRow>> {
    let mut rows = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let sql = format!(
            "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                    folders.path, images.file_size, images.width, images.height,
                    images.format, images.has_alpha, images.optimized_at
             FROM images JOIN folders ON folders.id = images.folder_id
             WHERE images.deleted_at IS NULL
               AND images.id IN ({})",
            crate::library::placeholders(chunk.len())
        );
        let mut part = database.with_reader(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok(CandidateRow {
                        id: row.get(0)?,
                        content_hash: row.get(1)?,
                        file_ext: row.get(2)?,
                        folder_id: row.get(3)?,
                        folder_path: row.get(4)?,
                        file_size: row.get(5)?,
                        width: row.get(6)?,
                        height: row.get(7)?,
                        format: row.get(8)?,
                        has_alpha: row.get::<_, i64>(9)? != 0,
                        optimized_at: row.get(10)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.append(&mut part);
    }
    Ok(rows)
}

/// 前置判据之后的候选张数与字节数；用于和估算凭证比对。
pub fn eligible_summary(candidates: &[CandidateRow], options: &OptimizeOptions) -> (u64, u64) {
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    for row in candidates {
        if optimize::prefilter(&row.source_info(), options).is_ok() {
            count += 1;
            bytes += row.file_size.max(0) as u64;
        }
    }
    (count, bytes)
}

/// 执行凭证：指纹一致，且候选集在估算值的 ±10% 以内。
pub fn verify_grant(
    estimate: &OptimizeEstimate,
    options_fingerprint: &str,
    candidate_count: u64,
    candidate_bytes: u64,
) -> AppResult<()> {
    if estimate.options_fingerprint != options_fingerprint {
        return Err(AppError::message(
            "估算结果已过期：范围或参数已改变，请重新估算",
        ));
    }
    if estimate.candidate_count == 0 || candidate_count == 0 {
        return Err(AppError::message("当前范围没有可压缩的图片，请重新估算"));
    }
    if !within_drift(estimate.candidate_count, candidate_count)
        || !within_drift(estimate.candidate_bytes, candidate_bytes)
    {
        return Err(AppError::message(
            "候选图片已变化，估算不再可靠，请重新估算",
        ));
    }
    Ok(())
}

/// 实际值与估算值相差不超过 10%。
fn within_drift(expected: u64, actual: u64) -> bool {
    let expected = expected.max(1) as f64;
    (actual as f64 - expected).abs() <= expected * 0.10
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeFailure {
    pub image_id: i64,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeReport {
    pub processed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub old_bytes: u64,
    pub new_bytes: u64,
    pub savings_bytes: u64,
    /// 连续失败过多而提前停止；字符串说明原因。
    pub stop_reason: Option<String>,
    pub cancelled: bool,
    pub failures: Vec<OptimizeFailure>,
}

/// 临时文件句柄：作用域结束时自动删除，成功发布后 `disarm` 交给目标文件。
struct TempPath(Option<PathBuf>);

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

    fn path(&self) -> &Path {
        self.0.as_deref().expect("临时文件路径在 disarm 前始终可用")
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

struct EncodedItem {
    row_id: i64,
    folder_path: String,
    old_hash: String,
    old_path: PathBuf,
    old_size: u64,
    new_hash: String,
    new_ext: &'static str,
    new_format: &'static str,
    new_size: u64,
    width: u32,
    height: u32,
    has_alpha: bool,
    temp: TempPath,
    thumb: Option<thumbnail::EncodedThumbnail>,
    phash: Option<[u8; 32]>,
    colors: Option<([u8; 16], [u8; 32])>,
}

struct Published {
    row_id: i64,
    old_hash: String,
    old_path: PathBuf,
    old_size: u64,
    new_size: u64,
}

/// 执行入口；`candidates` 由调用方按同一范围与参数收集，`operation_id` 是 live 记录。
pub fn optimize(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    candidates: Vec<CandidateRow>,
    options: &OptimizeOptions,
    operation_id: i64,
    reporter: &dyn BatchReporter,
) -> AppResult<OptimizeReport> {
    options.validate()?;
    let paths = LibraryPaths::new(root);
    std::fs::create_dir_all(&paths.temp)?;

    let mut report = OptimizeReport::default();
    let mut jobs: Vec<CandidateRow> = Vec::with_capacity(candidates.len());
    for row in candidates {
        if optimize::prefilter(&row.source_info(), options).is_err() {
            report.skipped += 1;
            continue;
        }
        jobs.push(row);
    }
    // 先压 bpp 高的图片：同样耗时时收益更大，与估算里的收益曲线顺序一致。
    jobs.sort_by(|left, right| right.bpp().total_cmp(&left.bpp()));

    let total = jobs.len();
    let pool = encode_pool("压缩")?;

    reporter.report(0, total);
    // 按张上报：整批 1000 张并行编码时也持续前进，用户能看到真实停在哪一张。
    let done = AtomicUsize::new(0);
    let mut consecutive_failures = 0_usize;
    let mut stopped = false;

    for (batch_index, batch) in jobs.chunks(BATCH_SIZE).enumerate() {
        if reporter.cancelled() {
            report.cancelled = true;
            break;
        }
        reporter.message("正在编码…");
        let results = pool.install(|| {
            batch
                .par_iter()
                .map(|row| {
                    // 取消后剩余的图不再编码，也不再计入进度：进度条停在真实张数。
                    if reporter.cancelled() {
                        return Ok(None);
                    }
                    let result = encode_one(&paths, row, options);
                    let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                    reporter.report(completed, total);
                    result
                })
                .collect::<Vec<AppResult<Option<EncodedItem>>>>()
        });

        let mut items = Vec::new();
        for (row, result) in batch.iter().zip(results) {
            match result {
                Ok(Some(item)) => {
                    items.push(item);
                    consecutive_failures = 0;
                }
                Ok(None) => {
                    report.skipped += 1;
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
                        report.stop_reason = Some("连续多张图片处理失败，任务已停止".to_owned());
                        stopped = true;
                    }
                }
            }
        }

        if !items.is_empty() {
            reporter.message(&format!("正在写入第 {} 批", batch_index + 1));
            let (published, duplicates) =
                commit_batch(database, &paths, thumbs, operation_id, items)?;
            // 目标内容已存在（别的行已占用同样字节）时不再落盘，按跳过计。
            report.skipped += duplicates;
            for item in published {
                report.processed += 1;
                report.old_bytes += item.old_size;
                report.new_bytes += item.new_size;
                // 旧哈希已不再被任何行引用（content_hash 唯一）；删除失败留给维护清理。
                if let Err(error) = metadata::remove_content_file(root, &item.old_path) {
                    log::warn!(
                        "旧原图清理失败 image={} path={}: {error}",
                        item.row_id,
                        item.old_path.display()
                    );
                }
                let _ =
                    std::fs::remove_file(viewer::preview_path(&paths, item.row_id, &item.old_hash));
            }
        }

        if reporter.cancelled() {
            report.cancelled = true;
        }
        if report.cancelled || stopped {
            break;
        }
    }

    report.savings_bytes = report.old_bytes.saturating_sub(report.new_bytes);
    Ok(report)
}

/// 失败清单里展示的路径；解析不出内容路径时退回「文件夹/哈希」。
fn display_path(paths: &LibraryPaths, row: &CandidateRow) -> String {
    metadata::content_path(paths, &row.folder_path, &row.content_hash, &row.file_ext)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| format!("{}/{}", row.folder_path, row.content_hash))
}

/// 单张编码：只写临时文件，不碰数据库与原图；未通过判据时返回 `None`。
fn encode_one(
    paths: &LibraryPaths,
    row: &CandidateRow,
    options: &OptimizeOptions,
) -> AppResult<Option<EncodedItem>> {
    let source_path =
        metadata::content_path(paths, &row.folder_path, &row.content_hash, &row.file_ext)?;
    let bytes = std::fs::read(&source_path)?;
    if optimize::color_profile_guard(&bytes, &row.format, options).is_err() {
        return Ok(None);
    }
    let decoded = decode::decode(&source_path)?;
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
        Err(_) => return Ok(None),
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
        LadderOutcome::Skipped(_) => return Ok(None),
    };
    let new_hash = blake3::hash(&candidate.bytes).to_hex().to_string();
    if new_hash == row.content_hash {
        return Ok(None);
    }

    let (width, height) = (prepared.width(), prepared.height());
    let temp_path = paths.temp.join(format!(
        "optimize-{}-{}.{}",
        row.id,
        uuid::Uuid::new_v4(),
        options.output_format.extension()
    ));
    let temp = TempPath::new(temp_path);
    {
        let mut file = std::fs::File::create(temp.path())?;
        file.write_all(&candidate.bytes)?;
        file.sync_all()?;
    }

    let thumb = if options.regenerate_thumbnails {
        Some(thumbnail::encode(&prepared)?)
    } else {
        None
    };
    let phash = options.recompute_phash.then(|| phash::compute(&prepared));
    let colors = options
        .recompute_colors
        .then(|| colors::dominant(&prepared));

    Ok(Some(EncodedItem {
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
    }))
}

/// 发布一批：事务内发布文件并更新行；提交失败时删掉已发布的新文件。
///
/// 返回已提交的条目，以及因目标内容已存在而没写入的张数——重复内容不落盘、也不改行。
fn commit_batch(
    database: &LibraryDb,
    paths: &LibraryPaths,
    thumbs: &ThumbPack,
    operation_id: i64,
    items: Vec<EncodedItem>,
) -> AppResult<(Vec<Published>, u64)> {
    let mut destinations = Vec::new();
    let result = database.with_writer(|connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let mut written = Vec::new();
        let mut duplicates = 0_u64;
        for mut item in items {
            let duplicate: Option<i64> = transaction
                .query_row(
                    "SELECT id FROM images WHERE content_hash = ?1 AND id <> ?2",
                    params![item.new_hash, item.row_id],
                    |row| row.get(0),
                )
                .optional()?;
            if duplicate.is_some() {
                // 已有相同内容：不重复落盘，临时文件随 item 一起删除。
                duplicates += 1;
                continue;
            }
            let destination =
                metadata::content_path(paths, &item.folder_path, &item.new_hash, item.new_ext)?;
            if !publish(item.temp.path(), &destination)? {
                // 目标已存在，说明内容重复；当作跳过处理。
                duplicates += 1;
                continue;
            }
            item.temp.disarm();
            destinations.push(destination);

            let changed = transaction.execute(
                "UPDATE images SET
                    content_hash = ?1, file_ext = ?2, file_size = ?3, width = ?4, height = ?5,
                    format = ?6, has_alpha = ?7, optimized_at = ?8
                 WHERE id = ?9 AND content_hash = ?10 AND deleted_at IS NULL",
                params![
                    item.new_hash,
                    item.new_ext,
                    i64::try_from(item.new_size)
                        .map_err(|_| AppError::message("压缩结果大小超出数据库范围"))?,
                    item.width,
                    item.height,
                    item.new_format,
                    item.has_alpha,
                    now,
                    item.row_id,
                    item.old_hash,
                ],
            )?;
            if changed == 0 {
                return Err(AppError::message("图片已被其他操作修改，已停止"));
            }

            if let Some(thumb) = &item.thumb {
                let location = thumbs.append(&thumb.bytes)?;
                transaction.execute(
                    "UPDATE images SET
                        thumb_segment = ?1, thumb_offset = ?2, thumb_length = ?3,
                        thumb_format = 'webp', thumb_width = ?4, thumb_height = ?5,
                        thumb_version = thumb_version + 1
                     WHERE id = ?6",
                    params![
                        location.segment,
                        i64::try_from(location.offset)
                            .map_err(|_| AppError::message("缩略图偏移超出数据库范围"))?,
                        location.length,
                        thumb.width,
                        thumb.height,
                        item.row_id,
                    ],
                )?;
                repository::ensure_thumb_segment(&transaction, location.segment, now)?;
                if let Ok(size) = thumbs.segment_size_bytes(location.segment) {
                    repository::update_thumb_segment_size(&transaction, location.segment, size)?;
                }
            }
            if let Some(hash) = item.phash {
                transaction.execute(
                    "UPDATE images SET phash = ?1 WHERE id = ?2",
                    params![hash.as_slice(), item.row_id],
                )?;
            }
            if let Some((colors4, colors8)) = item.colors {
                transaction.execute(
                    "UPDATE images SET colors4 = ?1, colors8 = ?2 WHERE id = ?3",
                    params![colors4.as_slice(), colors8.as_slice(), item.row_id],
                )?;
            }

            written.push(Published {
                row_id: item.row_id,
                old_hash: item.old_hash,
                old_path: item.old_path,
                old_size: item.old_size,
                new_size: item.new_size,
            });
        }
        // 只有确实写入内容的批次才结束重做分支：整批都是重复内容不算新时间线。
        if !written.is_empty() {
            crate::library::history::discard_redo_in_tx(&transaction)?;
        }
        crate::library::history::add_live_affected_in_tx(
            &transaction,
            operation_id,
            written.len() as u64,
        )?;
        transaction.commit()?;
        Ok((written, duplicates))
    });

    // 提交失败时删掉本批已发布的新文件：事务已回滚，没有行指向它们。
    result.inspect_err(|_| {
        for destination in destinations {
            let _ = std::fs::remove_file(destination);
        }
    })
}

/// 把临时文件移到目标内容路径；目标已存在（内容重复）时删掉临时文件并返回 `false`。
fn publish(temp: &Path, destination: &Path) -> AppResult<bool> {
    if destination.is_file() {
        std::fs::remove_file(temp)?;
        return Ok(false);
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(temp, destination) {
        Ok(()) => Ok(true),
        Err(_) if destination.is_file() => {
            let _ = std::fs::remove_file(temp);
            Ok(false)
        }
        Err(error) => Err(AppError::message(format!("发布压缩结果失败：{error}"))),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::PathBuf};

    use image::{DynamicImage, Rgb, RgbImage};
    use tempfile::tempdir;

    use super::*;
    use crate::library::{NoopReporter, history::NewOperation, service};

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        root: PathBuf,
    }

    fn fixture() -> (Fixture, i64, String, PathBuf) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "压缩测试", 1).unwrap();
        let mut image = RgbImage::new(160, 120);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            // 平滑渐变：JPEG 在较高画质下仍能大幅缩小，且 PSNR 足够高。
            *pixel = Rgb([
                (x * 255 / 159) as u8,
                (y * 255 / 119) as u8,
                ((x + y) * 255 / 278) as u8,
            ]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let path = root.join(format!("{hash}.png"));
        fs::write(&path, &bytes).unwrap();

        let image_id = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at, thumb_version
                     ) VALUES (?1, 'png', 1, ?2, 160, 120, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 4, '保留描述', 11, 11, 3)",
                    params![hash, bytes.len() as i64],
                )?;
                let id = connection.last_insert_rowid();
                connection.execute("INSERT INTO tags(name) VALUES ('保留标签')", [])?;
                connection.execute(
                    "INSERT INTO image_tags(image_id, tag_id)
                     SELECT ?1, id FROM tags WHERE name = '保留标签'",
                    [id],
                )?;
                connection.execute(
                    "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name,
                        pack_offset, pack_length, updated_at)
                     VALUES (?1, 'embedding', 'm', 3, 'f.bin', 0, 12, 1)",
                    [id],
                )?;
                Ok(id)
            })
            .unwrap();

        (
            Fixture {
                _temp: temp,
                session,
                root,
            },
            image_id,
            hash,
            path,
        )
    }

    #[derive(Default)]
    struct CountingReporter {
        calls: AtomicUsize,
    }

    impl crate::library::BatchReporter for CountingReporter {
        fn report(&self, _completed: usize, _total: usize) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }

        fn cancelled(&self) -> bool {
            false
        }
    }

    fn run(fixture: &Fixture, ids: &[i64], options: &OptimizeOptions) -> OptimizeReport {
        let operation = crate::library::history::begin_live(
            &fixture.session.db,
            NewOperation::new(
                crate::library::history::HistoryKind::Optimize,
                "优化图片空间",
            ),
        )
        .unwrap();
        let candidates = collect_candidates(&fixture.session.db, ids).expect("读取候选失败");
        let report = optimize(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            candidates,
            options,
            operation,
            &NoopReporter,
        )
        .unwrap();
        crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();
        report
    }

    #[test]
    fn grant_requires_matching_fingerprint_and_stable_candidates() {
        let mut estimate = OptimizeEstimate {
            options_fingerprint: "fp".to_owned(),
            candidate_count: 100,
            candidate_bytes: 1_000_000,
            ..OptimizeEstimate::default()
        };

        assert!(verify_grant(&estimate, "fp", 100, 1_000_000).is_ok());
        assert!(verify_grant(&estimate, "fp", 110, 1_100_000).is_ok());
        assert!(verify_grant(&estimate, "other", 100, 1_000_000).is_err());
        assert!(verify_grant(&estimate, "fp", 120, 1_000_000).is_err());
        assert!(verify_grant(&estimate, "fp", 100, 1_200_000).is_err());
        estimate.candidate_count = 0;
        assert!(verify_grant(&estimate, "fp", 0, 0).is_err());
    }

    #[test]
    fn compresses_content_and_keeps_metadata() {
        let (fixture, image_id, old_hash, old_path) = fixture();
        let report = run(&fixture, &[image_id], &OptimizeOptions::default());
        assert_eq!(report.processed, 1, "{report:?}");
        assert_eq!(report.failed, 0, "{report:?}");
        assert!(report.savings_bytes > 0);
        assert!(!old_path.exists());

        let (hash, size, optimized_at, score, description, imported, modified): (
            String,
            i64,
            i64,
            i64,
            String,
            i64,
            i64,
        ) = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT content_hash, file_size, optimized_at, score, description,
                            imported_at, modified_at
                     FROM images WHERE id = ?1",
                    [image_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )?)
            })
            .unwrap();
        assert_ne!(hash, old_hash);
        assert_eq!(size, report.new_bytes as i64);
        assert!(optimized_at > 0);
        assert_eq!((score, description.as_str()), (4, "保留描述"));
        assert_eq!((imported, modified), (11, 11));

        let extras: (i64, i64, i64) = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok((
                    connection
                        .query_row("SELECT count(*) FROM image_tags", [], |row| row.get(0))?,
                    connection
                        .query_row("SELECT count(*) FROM feature_blobs", [], |row| row.get(0))?,
                    connection.query_row(
                        "SELECT length(phash) FROM images WHERE id = ?1",
                        [image_id],
                        |row| row.get(0),
                    )?,
                ))
            })
            .unwrap();
        assert_eq!(extras, (1, 1, 32));

        let operation: (String, i64, String) = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT kind, affected, status FROM operations ORDER BY id DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .unwrap();
        assert_eq!(operation, ("optimize".to_owned(), 1, "applied".to_owned()));
    }

    #[test]
    fn reports_progress_per_image_instead_of_per_batch() {
        let (fixture, first_id, _, _) = fixture();
        let mut ids = vec![first_id];
        for index in 1..12_u32 {
            let mut image = RgbImage::new(96, 64);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                *pixel = Rgb([
                    ((x * 7 + index * 11) % 256) as u8,
                    ((y * 5 + index * 17) % 256) as u8,
                    ((x + y + index * 29) % 256) as u8,
                ]);
            }
            let mut bytes = Vec::new();
            DynamicImage::ImageRgb8(image)
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            fs::write(fixture.root.join(format!("{hash}.png")), &bytes).unwrap();
            let id = fixture
                .session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, 'png', 1, ?2, 96, 64, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        params![hash, bytes.len() as i64],
                    )?;
                    Ok(connection.last_insert_rowid())
                })
                .unwrap();
            ids.push(id);
        }

        let operation = crate::library::history::begin_live(
            &fixture.session.db,
            NewOperation::new(
                crate::library::history::HistoryKind::Optimize,
                "优化图片空间",
            ),
        )
        .unwrap();
        let candidates = collect_candidates(&fixture.session.db, &ids).unwrap();
        let reporter = CountingReporter::default();
        optimize(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            candidates.clone(),
            &OptimizeOptions::default(),
            operation,
            &reporter,
        )
        .unwrap();
        crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();

        // 一次初始上报 + 每张一次；按批上报只会是 1 次。
        assert_eq!(
            reporter.calls.load(Ordering::Relaxed),
            candidates.len() + 1,
            "进度没有按张上报"
        );
    }

    #[test]
    fn second_run_skips_already_optimized_then_allows_forced_reprocessing() {
        let (fixture, image_id, _, _) = fixture();
        let first = run(&fixture, &[image_id], &OptimizeOptions::default());
        assert_eq!(first.processed, 1);

        let second = run(&fixture, &[image_id], &OptimizeOptions::default());
        assert_eq!((second.processed, second.skipped), (0, 1), "{second:?}");

        let forced = run(
            &fixture,
            &[image_id],
            &OptimizeOptions {
                skip_optimized: false,
                ..OptimizeOptions::default()
            },
        );
        // 再次压缩要么成功，要么因画质/体积判据跳过；关键是不再被标记拦住。
        assert_eq!(forced.failed, 0, "{forced:?}");
    }

    #[test]
    fn pending_output_recovers_as_applied_after_crash() {
        let (fixture, image_id, old_hash, _) = fixture();
        let operation = crate::library::history::begin_live(
            &fixture.session.db,
            NewOperation::new(
                crate::library::history::HistoryKind::Optimize,
                "优化图片空间",
            ),
        )
        .unwrap();
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE operations SET affected = 1 WHERE id = ?1",
                    [operation],
                )?;
                connection.execute(
                    "UPDATE images SET optimized_at = 1 WHERE id = ?1",
                    [image_id],
                )?;
                Ok(())
            })
            .unwrap();

        crate::library::history::recover_on_open(&fixture.session.db).unwrap();
        let status: String = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT status FROM operations WHERE id = ?1",
                    [operation],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(status, "applied");
        assert_eq!(
            fixture
                .session
                .db
                .with_reader(|connection| {
                    let hash: String = connection.query_row(
                        "SELECT content_hash FROM images WHERE id = ?1",
                        [image_id],
                        |row| row.get(0),
                    )?;
                    Ok(hash)
                })
                .unwrap(),
            old_hash
        );
    }
}

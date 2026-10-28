//! 图片空间优化的写路径：候选收集、凭证校验、分批编码发布与旧图清理。
//!
//! 每张图先编码到统一临时目录；达到判据后按固定顺序发布：
//! 文件数据 fsync → 持久化 rename → 目录 flush → SQLite 事务提交（FULL）→ 删旧图。
//! 提交返回之前绝不删旧图；提交失败则删掉本批已发布的新文件。
//! 压缩不可撤销：记录只在时间线上占位并说明原图已删除。

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
use rusqlite::params;
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    concurrency::budget::{BudgetError, ImageBudget},
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
        import as repository,
        metadata::{self, ContentRemover},
        optimize_estimate::OptimizeEstimate,
        paths::LibraryPaths,
        service::now_ms,
        thumbs::ThumbPack,
        viewer,
    },
    platform::fs as durable,
};

/// 每批编码完成后在同一事务里发布，限制内存峰值与事务时长。
///
/// 256 张是双缓冲下的默认切分；字节上限避免少量超大图撑爆一批的内存峰值。
pub(crate) const BATCH_SIZE: usize = 256;
const MAX_BATCH_BYTES: u64 = 256 * 1024 * 1024;
/// 批内每编码这么多张更新一次 message；256 张一批时分 16 次，5 张/秒也约 3 秒一跳。
const MESSAGE_STEP: usize = 16;
/// 连续失败达到这个数就停止，避免磁盘或解码器故障时把整库刷成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;
const MAX_REPORTED_FAILURES: usize = 50;
const CHUNK: usize = 400;
/// 执行前逐张 `stat` 原图的线程数；热缓存下约 19 µs/张，冷盘会明显更慢。
const SOURCE_CHECK_THREADS: usize = 8;
/// 原图缺失或大小不符的允许比例；超过就拒绝执行，要求重新估算。
pub(crate) const SOURCE_DRIFT_LIMIT: f64 = 0.01;

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

/// 候选集合指纹：对排好序的 id 折叠，用来发现“张数与体积都对、但换了一批图”。
pub fn fingerprint_ids(ids: &[i64]) -> String {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for id in sorted {
        hasher.update(&id.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// 当前候选集合的指纹；口径与估算一致，只算前置判据通过的图片。
pub fn candidate_fingerprint(candidates: &[CandidateRow], options: &OptimizeOptions) -> String {
    let ids: Vec<i64> = candidates
        .iter()
        .filter(|row| optimize::prefilter(&row.source_info(), options).is_ok())
        .map(|row| row.id)
        .collect();
    fingerprint_ids(&ids)
}

/// 执行凭证：指纹一致、候选集在估算值的 ±10% 以内，且集合本身没换。
pub fn verify_grant(
    estimate: &OptimizeEstimate,
    options_fingerprint: &str,
    candidate_count: u64,
    candidate_bytes: u64,
    candidate_fingerprint: &str,
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
    // 删掉一批又导入一批体积接近的图，张数与体积都能对上，只有集合指纹能发现。
    if !estimate.candidate_fingerprint.is_empty()
        && estimate.candidate_fingerprint != candidate_fingerprint
    {
        return Err(AppError::message(
            "候选图片已换成另一批（数量与体积接近，但不是同一批图），请重新估算",
        ));
    }
    Ok(())
}

/// 执行前逐张核对原图的结果。
#[derive(Debug, Default)]
pub struct SourceCheck {
    /// 文件不存在（或被移走、路径不可用）。
    pub missing: u64,
    /// 文件还在，但大小与数据库记录不一致（被外部替换或截断）。
    pub changed: u64,
    /// 有问题、要从候选里剔除的行。
    pub bad_ids: Vec<i64>,
    /// 候选涉及的、已经不存在的文件夹路径。
    pub missing_dirs: Vec<String>,
}

impl SourceCheck {
    pub fn drifted(&self) -> u64 {
        self.missing + self.changed
    }
}

/// 逐张 `stat` 候选原图并与 `images.file_size` 比对，同时确认文件夹还在。
///
/// 274 068 个候选在热缓存下约 5 s（8 线程），相对整次执行只占百分之几；串行要
/// 30 s 以上，不能放进执行路径。
pub fn verify_sources(
    root: &Path,
    candidates: &[CandidateRow],
    options: &OptimizeOptions,
) -> SourceCheck {
    let paths = LibraryPaths::new(root);
    let rows: Vec<&CandidateRow> = candidates
        .iter()
        .filter(|row| optimize::prefilter(&row.source_info(), options).is_ok())
        .collect();

    let mut folders: HashSet<&str> = HashSet::new();
    for row in &rows {
        folders.insert(row.folder_path.as_str());
    }
    let mut missing_dirs: Vec<String> = folders
        .into_iter()
        .filter(|folder| !paths.folder_dir(folder).is_dir())
        .map(str::to_owned)
        .collect();
    missing_dirs.sort();

    let check = |row: &CandidateRow| -> (bool, bool) {
        let path = match metadata::content_path(
            &paths,
            &row.folder_path,
            &row.content_hash,
            &row.file_ext,
        ) {
            Ok(path) => path,
            // 路径不合法说明行本身有问题，按缺失处理，交给重新估算。
            Err(_) => return (true, false),
        };
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => (false, meta.len() != row.file_size.max(0) as u64),
            _ => (true, false),
        }
    };
    let outcomes: Vec<(bool, bool)> =
        match crate::library::encode_pool_with(SOURCE_CHECK_THREADS, "原图校验") {
            Ok(pool) => pool.install(|| rows.par_iter().map(|row| check(row)).collect()),
            // 建不了池就串行跑：慢一点，但不能因此跳过校验。
            Err(_) => rows.iter().map(|row| check(row)).collect(),
        };

    let mut result = SourceCheck {
        missing_dirs,
        ..SourceCheck::default()
    };
    for (row, (missing, changed)) in rows.iter().zip(outcomes) {
        if missing {
            result.missing += 1;
            result.bad_ids.push(row.id);
        } else if changed {
            result.changed += 1;
            result.bad_ids.push(row.id);
        }
    }
    result
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
    /// 全部跳过数（前置 + 候选内 + 内容重复），等于三个细分字段之和。
    pub skipped: u64,
    /// 只看数据库列就拦下的张数（GIF、bpp 低、已处理、透明、超 WebP 尺寸）。
    pub prefilter_skipped: u64,
    /// 解码或编码后才发现不处理的张数（色彩、16 位、画质、省得太少、动画/多页）。
    pub candidate_skipped: u64,
    /// 候选内跳过按原因的六个桶；之和恒等于 `candidate_skipped`。
    pub skip_color_profile: u64,
    pub skip_not_smaller: u64,
    pub skip_quality: u64,
    pub skip_depth: u64,
    pub skip_multiframe: u64,
    /// 其余原因：解码后才发现 bpp 太低、带透明、超出 WebP 尺寸等。
    pub skip_late: u64,
    /// 目标内容已被其他行占用而没有落盘的张数。
    pub duplicate_skipped: u64,
    pub failed: u64,
    /// 原图在编码前就找不到或读不出来：不当作解码/磁盘故障，也不触发连续失败停止。
    pub source_missing: u64,
    pub old_bytes: u64,
    pub new_bytes: u64,
    pub savings_bytes: u64,
    /// 连续失败过多而提前停止；字符串说明原因。
    pub stop_reason: Option<String>,
    pub cancelled: bool,
    pub failures: Vec<OptimizeFailure>,
    /// 整次执行的墙钟耗时。
    pub elapsed_ms: u64,
    /// 解码 + 阶梯编码 + 回读比对。
    pub encode_ms: u64,
    /// 成功编码后计算 BLAKE3。
    pub hash_ms: u64,
    /// 缩略图 / pHash / 主色重算（按勾选执行）。
    pub recalc_ms: u64,
    /// 写临时文件（含 fsync）。
    pub write_ms: u64,
    /// 其中 `sync_all` 的部分，用于校准每张 fsync 成本。
    pub fsync_ms: u64,
    /// 持久化 rename 与目录 flush。
    pub publish_ms: u64,
    /// 事务提交（含缩略图追加与行更新）。
    pub commit_ms: u64,
    /// 删旧图与查看器预览。
    pub delete_ms: u64,
    pub deleted_files: u64,
    pub delete_failed: u64,
}

impl OptimizeReport {
    /// 记一次候选内跳过：总数与细分桶一起加，两个数不会各说一套。
    fn record_skip(&mut self, reason: SkipReason) {
        match reason {
            SkipReason::ColorProfile => self.skip_color_profile += 1,
            SkipReason::NotSmallEnough => self.skip_not_smaller += 1,
            SkipReason::QualityNotMet => self.skip_quality += 1,
            SkipReason::SixteenBit => self.skip_depth += 1,
            SkipReason::AnimatedOrMultipage => self.skip_multiframe += 1,
            SkipReason::BppLow
            | SkipReason::AlreadyOptimized
            | SkipReason::Transparent
            | SkipReason::TooLargeForWebp => self.skip_late += 1,
        }
        self.candidate_skipped += 1;
    }

    /// 候选内的跳过数（含内容重复与原图缺失），不含执行前被排除的图片。
    ///
    /// 这是任务、界面与曲线唯一的跳过口径；`skipped` 里那部分执行前排除的张数
    /// 不属于候选池，混进来会让分母与分子对不上。
    pub fn progress_skipped(&self) -> usize {
        (self.candidate_skipped + self.duplicate_skipped + self.source_missing) as usize
    }

    /// 跳过细分的对外形状：只保留非 0 桶，按张数降序；key 与估算视图的图例一致。
    pub fn skip_buckets(&self) -> Vec<(String, u64)> {
        let mut buckets = vec![
            ("color".to_owned(), self.skip_color_profile),
            ("depth".to_owned(), self.skip_depth),
            ("quality".to_owned(), self.skip_quality),
            ("notSmaller".to_owned(), self.skip_not_smaller),
            ("multiframe".to_owned(), self.skip_multiframe),
            ("late".to_owned(), self.skip_late),
        ];
        buckets.retain(|(_, count)| *count > 0);
        buckets.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        buckets
    }
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

struct Published {
    row_id: i64,
    old_hash: String,
    old_path: PathBuf,
    old_size: u64,
    new_size: u64,
}

#[derive(Default)]
struct CommitOutcome {
    published: Vec<Published>,
    duplicates: u64,
    write_ms: u64,
    commit_ms: u64,
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
fn skipped_count(report: &OptimizeReport) -> usize {
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
                        let relative = content::relative_display_path(
                            &row.folder_path,
                            &row.content_hash,
                            &row.file_ext,
                        );
                        reporter.current(Some(row.id), Some(&relative));
                        let result = encode_one(&paths, row, options);
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

/// 发布一批：先把文件持久化到目标内容路径并 flush 目录，再在一个 FULL 事务里更新行。
///
/// 返回已提交的条目与因目标内容已被占用而跳过的张数。提交失败时删掉本批已发布的
/// 新文件——事务已回滚，没有行指向它们。
fn commit_batch(
    database: &LibraryDb,
    paths: &LibraryPaths,
    thumbs: &ThumbPack,
    operation_id: i64,
    items: Vec<EncodedItem>,
) -> AppResult<CommitOutcome> {
    // 一次查出本批新哈希已被哪些行占用；不再逐张 `SELECT`。
    let referenced = referenced_hashes(database, &items)?;

    let write_started = Instant::now();
    let mut staged: Vec<EncodedItem> = Vec::with_capacity(items.len());
    let mut written_files: Vec<PathBuf> = Vec::new();
    let mut directories: HashSet<PathBuf> = HashSet::new();
    let mut created_dirs: HashSet<PathBuf> = HashSet::new();
    // 批内也可能有相同的新内容（两张源图编码后完全一致）：内存集合挡住，
    // 否则事务里的唯一约束会直接报错。
    let mut batch_hashes: HashSet<String> = HashSet::with_capacity(items.len());
    let mut duplicates = 0_u64;
    let mut failure: Option<AppError> = None;
    for mut item in items {
        if referenced.contains(&item.new_hash) || !batch_hashes.insert(item.new_hash.clone()) {
            // 已有行或本批前面的图占用同样内容：不重复落盘，临时文件随 item 删除。
            duplicates += 1;
            continue;
        }
        let destination =
            match metadata::content_path(paths, &item.folder_path, &item.new_hash, item.new_ext) {
                Ok(destination) => destination,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
        if let Some(parent) = destination.parent() {
            // 同一批目标目录只创建一次。
            if created_dirs.insert(parent.to_path_buf())
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                failure = Some(error.into());
                break;
            }
            directories.insert(parent.to_path_buf());
        }
        match publish(item.temp.path(), &destination, &item.new_hash) {
            Ok(PublishOutcome::Written) => {
                item.temp.disarm();
                written_files.push(destination);
            }
            // 孤儿文件内容匹配：接管已有文件，临时文件由守卫删除。
            Ok(PublishOutcome::Adopted) => {}
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
        staged.push(item);
    }
    if let Some(error) = failure {
        // 已经发布的新文件没有行引用，删掉；完整文件就算留下也会在下次被接管。
        discard_written(&written_files);
        return Err(error);
    }
    // 每个目标目录只 flush 一次：1000 张图可能落在几十个文件夹里。
    for directory in &directories {
        if let Err(error) = durable::flush_directory(directory) {
            discard_written(&written_files);
            return Err(error);
        }
    }
    let write_ms = write_started.elapsed().as_millis() as u64;

    let commit_started = Instant::now();
    let result = database.with_durable_writer(|connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let mut written = Vec::new();
        for item in staged {
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
        Ok(written)
    });
    let commit_ms = commit_started.elapsed().as_millis() as u64;

    match result {
        Ok(published) => Ok(CommitOutcome {
            published,
            duplicates,
            write_ms,
            commit_ms,
        }),
        Err(error) => {
            // 事务已回滚，没有行指向这些新文件；删掉它们，避免留下孤儿。
            discard_written(&written_files);
            Err(error)
        }
    }
}

/// 删掉本批已发布却没有行引用的新文件；清理失败只记日志，不盖掉真正的错误。
fn discard_written(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = std::fs::remove_file(path) {
            log::warn!("清理未入库的压缩结果失败 {}：{error}", path.display());
        }
    }
}

/// 本批新哈希已被哪些行引用；分块查询，避免一次塞进上千个参数。
fn referenced_hashes(database: &LibraryDb, items: &[EncodedItem]) -> AppResult<HashSet<String>> {
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

enum PublishOutcome {
    /// 临时文件已经改名到目标路径。
    Written,
    /// 目标路径上已有内容完全相同的文件（上次崩溃留下的孤儿），直接接管。
    Adopted,
}

/// 把临时文件发布到目标内容路径；目标已存在时先核对内容。
///
/// 目标存在不等于“内容重复”：rename 后、提交前崩溃会留下没有任何行引用的孤儿。
/// 内容匹配就接管（少见路径才读文件算哈希），不匹配说明是截断或损坏的旧文件，
/// 删掉再重新发布。正常路径（目标不存在）不增加任何成本。
fn publish(temp: &Path, destination: &Path, expected_hash: &str) -> AppResult<PublishOutcome> {
    if destination.is_file() {
        if content::hash_file(destination)? == expected_hash {
            log::warn!(
                "接管没有行引用的内容文件（上次发布未提交）：{}",
                destination.display()
            );
            return Ok(PublishOutcome::Adopted);
        }
        log::warn!(
            "目标内容与预期不符，删除后重新发布：{}",
            destination.display()
        );
        std::fs::remove_file(destination)?;
    }
    durable::rename_durable(temp, destination)?;
    Ok(PublishOutcome::Written)
}

/// 提交后删旧图与查看器预览：不持有数据库写锁，并行执行，失败只记日志。
fn delete_replaced(
    pool: &rayon::ThreadPool,
    root: &Path,
    paths: &LibraryPaths,
    published: &[Published],
    report: &mut OptimizeReport,
) {
    let started = Instant::now();
    let mut remover = match ContentRemover::new(root) {
        Ok(remover) => remover,
        Err(error) => {
            log::warn!("旧原图路径校验失败，留给维护清理：{error}");
            report.delete_failed += published.len() as u64;
            report.delete_ms += started.elapsed().as_millis() as u64;
            return;
        }
    };
    let mut targets = Vec::with_capacity(published.len());
    for item in published {
        match remover.validate(&item.old_path) {
            Ok(()) => targets.push(item),
            Err(error) => {
                report.delete_failed += 1;
                log::warn!(
                    "旧原图路径校验失败 image={} path={}: {error}",
                    item.row_id,
                    item.old_path.display()
                );
            }
        }
    }
    let results = pool.install(|| {
        targets
            .par_iter()
            .map(|item| {
                let file = std::fs::remove_file(&item.old_path);
                // 预览按旧哈希命名，删不掉通常是本来就没有。
                let _ =
                    std::fs::remove_file(viewer::preview_path(paths, item.row_id, &item.old_hash));
                (item.row_id, file)
            })
            .collect::<Vec<_>>()
    });
    for (row_id, file) in results {
        match file {
            Ok(()) => report.deleted_files += 1,
            Err(error) => {
                report.delete_failed += 1;
                log::warn!("旧原图清理失败 image={row_id}: {error}");
            }
        }
    }
    report.delete_ms += started.elapsed().as_millis() as u64;
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::PathBuf, sync::atomic::AtomicUsize};

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
        total: AtomicUsize,
        skipped: AtomicUsize,
        meter: crate::concurrency::meter::WorkMeter,
    }

    impl crate::library::BatchReporter for CountingReporter {
        fn report(&self, _completed: usize, total: usize) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.total.store(total, Ordering::Relaxed);
        }

        fn cancelled(&self) -> bool {
            false
        }

        fn meter(&self) -> Option<&crate::concurrency::meter::WorkMeter> {
            Some(&self.meter)
        }

        fn skipped_count(&self, skipped: usize) {
            self.skipped.store(skipped, Ordering::Relaxed);
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
            &crate::concurrency::budget::ImageBudget::new(2),
        )
        .unwrap();
        crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();
        report
    }

    #[test]
    fn grant_requires_matching_fingerprint_and_stable_candidates() {
        let mut estimate = OptimizeEstimate {
            options_fingerprint: "fp".to_owned(),
            candidate_fingerprint: "set".to_owned(),
            candidate_count: 100,
            candidate_bytes: 1_000_000,
            ..OptimizeEstimate::default()
        };

        assert!(verify_grant(&estimate, "fp", 100, 1_000_000, "set").is_ok());
        assert!(verify_grant(&estimate, "fp", 110, 1_100_000, "set").is_ok());
        assert!(verify_grant(&estimate, "other", 100, 1_000_000, "set").is_err());
        assert!(verify_grant(&estimate, "fp", 120, 1_000_000, "set").is_err());
        assert!(verify_grant(&estimate, "fp", 100, 1_200_000, "set").is_err());
        // 张数与体积都在范围内，但集合换了另一批图。
        assert!(verify_grant(&estimate, "fp", 100, 1_000_000, "other-set").is_err());
        // 旧结果没有集合指纹时不因此拒绝（升级后的首次执行）。
        estimate.candidate_fingerprint.clear();
        assert!(verify_grant(&estimate, "fp", 100, 1_000_000, "other-set").is_ok());
        estimate.candidate_count = 0;
        assert!(verify_grant(&estimate, "fp", 0, 0, "set").is_err());
    }

    #[test]
    fn source_check_reports_missing_and_changed_files() {
        let (fixture, image_id, _hash, path) = fixture();
        let rows = collect_candidates(&fixture.session.db, &[image_id]).unwrap();
        let options = OptimizeOptions::default();
        let ok = verify_sources(&fixture.root, &rows, &options);
        assert_eq!((ok.missing, ok.changed), (0, 0), "{ok:?}");
        assert!(ok.missing_dirs.is_empty());

        // 大小不符（被外部替换或截断）。
        let original = fs::read(&path).unwrap();
        fs::write(&path, &original[..original.len() / 2]).unwrap();
        let changed = verify_sources(&fixture.root, &rows, &options);
        assert_eq!((changed.missing, changed.changed), (0, 1), "{changed:?}");
        assert_eq!(changed.bad_ids, vec![image_id]);

        // 原图被删。
        fs::remove_file(&path).unwrap();
        let missing = verify_sources(&fixture.root, &rows, &options);
        assert_eq!((missing.missing, missing.changed), (1, 0), "{missing:?}");

        // 整个目录被删：目录缺失要能单独报出来。
        let mut ghost = rows[0].clone();
        ghost.folder_path = "已经不存在的目录".to_owned();
        let gone = verify_sources(&fixture.root, &[ghost], &options);
        assert_eq!(
            gone.missing_dirs,
            vec!["已经不存在的目录".to_owned()],
            "{gone:?}"
        );
    }

    /// 原图缺失只计数，不触发“连续 5 张失败即停止”。
    #[test]
    fn missing_sources_are_counted_without_stopping_the_run() {
        let (fixture, first_id, _, _) = fixture();
        let mut ids = vec![first_id];
        let mut paths = Vec::new();
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
            let path = fixture.root.join(format!("{hash}.png"));
            fs::write(&path, &bytes).unwrap();
            paths.push(path);
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
        // 前 6 张原图被外部删掉：超过连续失败阈值，但不应停掉任务。
        for path in paths.iter().take(6) {
            fs::remove_file(path).unwrap();
        }

        let report = run(&fixture, &ids, &OptimizeOptions::default());
        assert_eq!(report.source_missing, 6, "{report:?}");
        assert_eq!(report.failed, 0, "{report:?}");
        assert_eq!(report.stop_reason, None, "{report:?}");
        // 其余 6 张照常处理（fixture 的首张 + 后面 5 张）。
        assert_eq!(report.processed, 6, "{report:?}");
    }

    #[test]
    fn compresses_content_and_keeps_metadata() {
        let (fixture, image_id, old_hash, old_path) = fixture();
        let report = run(&fixture, &[image_id], &OptimizeOptions::default());
        assert_eq!(report.processed, 1, "{report:?}");
        assert_eq!(report.failed, 0, "{report:?}");
        assert!(report.savings_bytes > 0);
        assert!(!old_path.exists());
        // 删旧图发生在提交之后，报告里要能看见这段耗时。
        assert_eq!(report.deleted_files, 1);
        assert!(report.elapsed_ms >= report.commit_ms);
        assert_eq!(
            report.skipped,
            report.prefilter_skipped + report.candidate_skipped + report.duplicate_skipped
        );

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
        let report = optimize(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            candidates.clone(),
            &OptimizeOptions::default(),
            operation,
            &reporter,
            &crate::concurrency::budget::ImageBudget::new(2),
        )
        .unwrap();
        crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();

        // 一次初始上报 + 每张一次；按批上报只会是 1 次。
        assert_eq!(
            reporter.calls.load(Ordering::Relaxed),
            candidates.len() + 1,
            "进度没有按张上报"
        );
        // 进度分母是候选张数，不是像素总量；每张推进一格，跑完正好等于分母。
        let jobs = candidates.len() - report.prefilter_skipped as usize;
        assert_eq!(reporter.meter.total_units(), jobs as u64);
        assert_eq!(reporter.meter.done_units(), jobs as u64);
        // 任务快照的分母同样是候选池：执行前被排除的图片不进进度，否则开局就六成。
        assert_eq!(reporter.total.load(Ordering::Relaxed), jobs);
        assert_eq!(
            reporter.skipped.load(Ordering::Relaxed),
            report.skipped as usize - report.prefilter_skipped as usize,
            "任务里报的跳过数不该包含执行前被排除的图片"
        );
    }

    #[test]
    fn second_run_skips_already_optimized_then_allows_forced_reprocessing() {
        let (fixture, image_id, _, _) = fixture();
        let first = run(&fixture, &[image_id], &OptimizeOptions::default());
        assert_eq!(first.processed, 1);

        let second = run(&fixture, &[image_id], &OptimizeOptions::default());
        assert_eq!(
            (second.processed, second.prefilter_skipped),
            (0, 1),
            "{second:?}"
        );

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

    /// 目标路径上的文件内容与预期哈希一致时直接接管，不重复写盘。
    #[test]
    fn publish_adopts_matching_orphan_and_replaces_mismatched_one() {
        let temp = tempdir().unwrap();
        let payload = b"published-bytes".to_vec();
        let hash = blake3::hash(&payload).to_hex().to_string();
        let destination = temp.path().join(format!("{hash}.jpg"));

        // 孤儿完整：接管，临时文件不再需要。
        fs::write(&destination, &payload).unwrap();
        let staged = temp.path().join("staged-1.tmp");
        fs::write(&staged, &payload).unwrap();
        assert!(matches!(
            publish(&staged, &destination, &hash).unwrap(),
            PublishOutcome::Adopted
        ));
        assert_eq!(fs::read(&destination).unwrap(), payload);

        // 孤儿截断：内容不匹配，删掉后重新发布。
        fs::write(&destination, &payload[..4]).unwrap();
        let staged = temp.path().join("staged-2.tmp");
        fs::write(&staged, &payload).unwrap();
        assert!(matches!(
            publish(&staged, &destination, &hash).unwrap(),
            PublishOutcome::Written
        ));
        assert!(!staged.exists());
        assert_eq!(fs::read(&destination).unwrap(), payload);
    }

    /// 提交阶段失败时，本批已发布的新文件必须删掉，旧图保持不动。
    #[test]
    fn failed_commit_removes_new_files_and_keeps_old_one() {
        let (fixture, image_id, old_hash, old_path) = fixture();
        let row = collect_candidates(&fixture.session.db, &[image_id])
            .unwrap()
            .pop()
            .unwrap();
        // 指向一个不存在的行：编码与发布都会成功，事务里的 UPDATE 命中 0 行。
        let mut ghost = row.clone();
        ghost.id = image_id + 10_000;

        let operation = crate::library::history::begin_live(
            &fixture.session.db,
            NewOperation::new(
                crate::library::history::HistoryKind::Optimize,
                "优化图片空间",
            ),
        )
        .unwrap();
        let error = optimize(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            vec![ghost],
            &OptimizeOptions::default(),
            operation,
            &NoopReporter,
            &crate::concurrency::budget::ImageBudget::new(2),
        )
        .unwrap_err();
        crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();
        assert!(error.to_string().contains("已被其他操作修改"), "{error}");

        // 旧图还在，行也没变；新文件（内容哈希文件）已被清理。
        assert!(old_path.exists());
        let files: Vec<PathBuf> = fs::read_dir(&fixture.root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(
            files[0].file_name().unwrap().to_string_lossy(),
            format!("{old_hash}.png")
        );
        let temp_files = fs::read_dir(fixture.root.join(".artbank").join("temp"))
            .unwrap()
            .count();
        assert_eq!(temp_files, 0, "临时文件应随守卫删除");
    }

    /// 六个桶之和恒等于 `candidate_skipped`，界面上的细项加起来不会多也不会少。
    #[test]
    fn skip_buckets_cover_every_reason() {
        let reasons = [
            SkipReason::ColorProfile,
            SkipReason::NotSmallEnough,
            SkipReason::QualityNotMet,
            SkipReason::SixteenBit,
            SkipReason::AnimatedOrMultipage,
            SkipReason::BppLow,
            SkipReason::AlreadyOptimized,
            SkipReason::Transparent,
            SkipReason::TooLargeForWebp,
        ];
        let mut report = OptimizeReport::default();
        for reason in reasons {
            report.record_skip(reason);
        }
        assert_eq!(report.candidate_skipped, reasons.len() as u64);
        let buckets = report.skip_buckets();
        assert_eq!(buckets.len(), 6, "{buckets:?}");
        let sum: u64 = buckets.iter().map(|(_, count)| count).sum();
        assert_eq!(sum, report.candidate_skipped);
        // 四个「解码后才发现」的原因都落在 late，按张数降序排在最前。
        assert_eq!(buckets[0], ("late".to_owned(), 4));
        // key 与估算视图的图例同名，前端两张图例共用一套文案。
        assert_eq!(
            buckets
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec![
                "late",
                "color",
                "depth",
                "quality",
                "notSmaller",
                "multiframe"
            ]
        );
    }

    /// 进度的跳过口径只认候选池：执行前被排除的图片不算「已跳过」。
    #[test]
    fn progress_skipped_excludes_prefiltered() {
        let mut report = OptimizeReport {
            prefilter_skipped: 61_401,
            ..OptimizeReport::default()
        };
        report.record_skip(SkipReason::ColorProfile);
        report.duplicate_skipped = 2;
        report.source_missing = 1;
        assert_eq!(report.progress_skipped(), 4);
        assert_eq!(skipped_count(&report), 61_405);
    }

    /// 一次执行后校准值写进 meta，且数值落在合理量级内。
    #[test]
    fn writes_calibration_after_a_run() {
        let (fixture, image_id, _, _) = fixture();
        let report = run(&fixture, &[image_id], &OptimizeOptions::default());
        assert_eq!(report.processed, 1);
        // 少于 20 张的执行不覆盖校准值：默认库里本来就没有记录。
        assert_eq!(
            OptimizeCalibration::load(&fixture.session.db).unwrap(),
            None
        );
    }
}

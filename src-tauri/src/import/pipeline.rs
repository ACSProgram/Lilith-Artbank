use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use rusqlite::Connection;
use uuid::Uuid;

use crate::{
    app::error::{AppError, AppResult},
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget, estimate_working_bytes},
        resources::{HolderInfo, ResourceTable},
    },
    imageops::{
        colors,
        optimize::{self, LadderOutcome, OptimizeFormat, OptimizeOptions, SourceInfo},
        phash, thumbnail,
    },
    import::{
        FolderMode, ImportRequest, ImportSource, ImportTarget, decode,
        job::{DuplicatePolicy, ImportHint, ImportJob, ImportPhase},
        scan,
    },
    library::{
        content,
        db::LibraryDb,
        history::{self, FolderRef, ImportMerged, ImportRecord, ImportTag},
        import as repository,
        import::ImageInsert,
        paths::{LibraryPaths, join_folder, normalize_folder},
        service::now_ms,
        thumbs::ThumbPack,
    },
    platform::fs as durable,
};

const BATCH_SIZE: usize = 64;
const DUPLICATE_BATCH: usize = 128;
const CHANNEL_DEPTH: usize = 128;
const READ_BUFFER: usize = 1024 * 1024;

/// 库内 pHash 索引：`(image_id, 4×u64)`，按 id 追加，供感知重复线性扫描。
type PhashIndex = Vec<(i64, [u64; 4])>;

struct TempGuard(PathBuf);

impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct WriterOptions {
    tags: Vec<String>,
    score: u8,
    policy: DuplicatePolicy,
    operation_id: Option<i64>,
    /// 临时导入固定使用该文件夹 id，不再按路径解析。
    folder_id: Option<i64>,
    /// 图库导入时丢弃占着同内容哈希的临时行；临时导入保持它们之间的查重。
    replace_temp_duplicate: bool,
    /// 临时文件夹的物理目录；丢弃临时行后用它清掉原图副本。
    temp_content_dir: Option<PathBuf>,
    /// 临时文件夹相对路径，用于丢弃临时行时短暂申请路径写锁。
    temp_folder: String,
    resources: Arc<ResourceTable>,
    holder: HolderInfo,
}

struct Staged {
    path: PathBuf,
    hash: String,
    size: u64,
}

struct Prepared {
    source_path: PathBuf,
    destination: PathBuf,
    folder: String,
    published: bool,
    content_hash: String,
    file_ext: String,
    file_size: u64,
    width: u32,
    height: u32,
    format: String,
    has_alpha: bool,
    phash: [u8; 32],
    colors4: [u8; 16],
    colors8: [u8; 32],
    thumb: thumbnail::EncodedThumbnail,
    similar: Option<(i64, u32)>,
    optimized_at: i64,
    /// 导入时压缩成功后省下的字节数；`None` 表示保留原图。
    optimized_savings: Option<u64>,
}

/// 工作线程交给写线程的结果。`Prepared` 是热点载荷，装箱会为每张图多一次分配，
/// 因此保留内联；通道深度 256，峰值占用约 75 KB。
#[allow(clippy::large_enum_variant)]
enum Outcome {
    Prepared(Prepared),
    Duplicate(DuplicateOutcome),
}

struct DuplicateOutcome {
    hash: String,
    source_path: PathBuf,
    in_batch: bool,
    /// 命中库内已有图片时的 id 与是否在回收站；批内重复没有这两个值。
    existing_id: Option<i64>,
    trashed: bool,
    /// 批内重复的胜出源文件；用于回执里的 keptPath。
    kept_path: Option<PathBuf>,
}

struct WorkerContext {
    job: Arc<ImportJob>,
    database: Arc<LibraryDb>,
    paths: LibraryPaths,
    temp_dir: PathBuf,
    folder: String,
    folder_mode: FolderMode,
    /// 查重时是否把临时区的行算作「已存在」；只有临时导入才看临时区。
    dedup_includes_temp: bool,
    threshold: u8,
    /// 勾选「导入时压缩」后的编码参数；`None` 表示原样入库。
    optimize: Option<OptimizeOptions>,
    /// 已占位的内容哈希 → 首个源文件；用于批内重复归因，跨 flush 保留。
    inflight: Arc<Mutex<HashMap<String, PathBuf>>>,
    index: Arc<RwLock<PhashIndex>>,
    sender: mpsc::SyncSender<Outcome>,
    images: Arc<ImageBudget>,
    memory: Arc<ImageMemoryBudget>,
    /// 本任务单张图片的最大估算；只用于收尾日志。
    max_estimate: AtomicU64,
    #[cfg(test)]
    cancel_after: Option<usize>,
}

struct InsertedRow {
    id: i64,
    similar: Option<(i64, u32)>,
    phash: [u8; 32],
}

struct CommitStats {
    inserted: Vec<InsertedRow>,
    failures: Vec<(String, String)>,
    duplicates: usize,
    merged: usize,
    /// 图库导入时被丢弃的、占着同内容哈希的临时行。
    temp_replaced: usize,
    optimized: usize,
    optimized_bytes: u64,
    /// 本批新增行（不含重复与合并）的最终字节总和。
    written_bytes: u64,
    duplicate_pairs: Vec<(String, i64, bool)>,
    batch_duplicate_pairs: Vec<(String, String)>,
    merged_image_ids: Vec<i64>,
    imported_pairs: Vec<(String, i64)>,
    /// 临时行删掉后留在临时目录里的原图，提交成功后再清理。
    orphan_temp_files: Vec<PathBuf>,
}

pub fn run(source: ImportSource, job: Arc<ImportJob>, request: ImportRequest) -> AppResult<()> {
    let paths = LibraryPaths::new(&source.root);
    let temp_dir = paths.temp.join(format!("import-{}", job.id()));
    std::fs::create_dir_all(&temp_dir)?;
    let _guard = TempGuard(temp_dir.clone());
    run_inner(&source, &job, &request, &paths, &temp_dir)
}

fn run_inner(
    source: &ImportSource,
    job: &Arc<ImportJob>,
    request: &ImportRequest,
    paths: &LibraryPaths,
    temp_dir: &Path,
) -> AppResult<()> {
    job.set_phase(ImportPhase::Scanning);
    job.set_current("扫描输入目录…");

    let inputs: Vec<PathBuf> = request.paths.iter().map(PathBuf::from).collect();
    let scanned = scan::collect(&inputs, &source.root, || job.should_cancel());
    for failure in &scanned.failures {
        job.add_failure(failure.path.clone(), failure.reason.clone());
        job.inc_failed();
    }
    if scanned.truncated {
        job.add_failure("", "文件数量超过单次导入上限 100000，只处理了前面的文件");
        job.inc_failed();
    }
    job.set_total(scanned.files.len());
    if job.should_cancel() || scanned.files.is_empty() {
        job.set_phase(ImportPhase::Finishing);
        return Ok(());
    }

    job.set_phase(ImportPhase::Processing);

    let hashes = source.database.with_reader(repository::load_phashes)?;
    let index = Arc::new(RwLock::new(
        hashes
            .into_iter()
            .map(|(id, hash)| (id, phash::words(&hash)))
            .collect::<Vec<_>>(),
    ));

    let temp_folder = source
        .database
        .with_reader(crate::library::temp::path)?
        .unwrap_or_default();
    let (folder, folder_id, temp_content_dir) = match request.target {
        ImportTarget::Library => {
            // 图库导入也可能要丢弃临时区的同内容行，提前拿到临时目录好清理副本。
            let directory = source
                .database
                .with_reader(crate::library::temp::path)?
                .map(|path| paths.folder_dir(&path));
            (request.folder.clone(), None, directory)
        }
        ImportTarget::Temp => {
            let temp = source.database.with_reader(crate::library::temp::require)?;
            let directory = paths.folder_dir(&temp.path);
            (temp.path, Some(temp.id), Some(directory))
        }
    };

    let inflight = Arc::new(Mutex::new(HashMap::new()));
    let (sender, receiver) = mpsc::sync_channel::<Outcome>(CHANNEL_DEPTH);
    let writer_options = WriterOptions {
        tags: request.tags.clone(),
        score: request.score,
        policy: request.duplicate_policy,
        operation_id: source.operation_id,
        folder_id,
        replace_temp_duplicate: request.target == ImportTarget::Library,
        temp_content_dir,
        temp_folder,
        resources: source.resources.clone(),
        holder: source.holder.clone(),
    };

    // 工作池必须在写线程启动前建好：建池失败提前返回时 sender 还活着，
    // WriterGuard 的 Drop 会等一个永远不结束的写线程。
    let threads = source
        .images
        .current_limit()
        .min(scanned.files.len().max(1));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("import-{index}"))
        .build()
        .map_err(|error| AppError::message(format!("无法创建导入工作池：{error}")))?;

    let mut writer = WriterGuard::new({
        let database = source.database.clone();
        let pack = source.thumbs.clone();
        let job = job.clone();
        let index = index.clone();
        std::thread::spawn(move || {
            write_loop(receiver, database, pack, writer_options, job, index);
        })
    });

    {
        let context = WorkerContext {
            job: job.clone(),
            database: source.database.clone(),
            paths: paths.clone(),
            temp_dir: temp_dir.to_path_buf(),
            folder,
            folder_mode: request.folder_mode,
            dedup_includes_temp: request.target == ImportTarget::Temp,
            threshold: request.perceptual_threshold,
            optimize: request.optimize.map(|value| value.options()),
            inflight,
            index,
            sender,
            images: source.images.clone(),
            memory: source.memory.clone(),
            max_estimate: AtomicU64::new(0),
            #[cfg(test)]
            cancel_after: request.cancel_after,
        };
        let files = &scanned.files;
        let next = AtomicUsize::new(0);
        pool.install(|| {
            (0..threads).into_par_iter().for_each(|_| {
                loop {
                    if job.should_cancel() {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(index) else {
                        break;
                    };
                    process_one(&file.path, &file.relative_folder, &context);
                }
            });
        });
        let mib = 1024 * 1024;
        log::debug!(
            "导入内存预算：{threads} 线程，全进程上限 {} MiB，全进程峰值占用 {} MiB，单张最大估算 {} MiB",
            context.memory.limit() / mib,
            context.memory.peak() / mib,
            context.max_estimate.load(Ordering::Relaxed) / mib,
        );
    }

    job.set_phase(ImportPhase::Finishing);
    job.set_current("正在写入最后一批…");
    match writer.join() {
        Ok(()) => Ok(()),
        Err(_) => {
            job.add_failure("", "入库线程异常终止，部分结果可能未写入");
            job.inc_failed();
            Ok(())
        }
    }
}

/// 入库线程的 join 守卫。
///
/// 工作线程 panic 会在 `pool.install` 处展开，正常路径的 `join` 就执行不到了；
/// 写线程仍持有通道接收端，会继续把已排队的结果写进库，甚至在本任务收尾之后还在写。
/// 交给 Drop 兜底，保证任何退出路径都先排空写线程，再让调用方 `finish`。
/// 写线程只在 sender 全部释放后退出，所以 `WriterGuard` 必须声明在 `context`（持有
/// sender）之前：drop 时 context 先释放，最后才轮到 guard 的 join。
struct WriterGuard(Option<JoinHandle<()>>);

impl WriterGuard {
    fn new(handle: JoinHandle<()>) -> Self {
        Self(Some(handle))
    }

    /// 显式等待；重复调用直接返回成功。
    fn join(&mut self) -> std::thread::Result<()> {
        match self.0.take() {
            Some(handle) => handle.join(),
            None => Ok(()),
        }
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        if self.join().is_err() {
            log::error!("导入入库线程异常终止");
        }
    }
}

fn process_one(path: &Path, relative_folder: &str, context: &WorkerContext) {
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

fn copy_and_hash(source: &Path, destination: &Path) -> AppResult<(String, u64)> {
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
fn publish(temp: &Path, destination: &Path, expected_hash: &str) -> AppResult<bool> {
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
fn write_encoded(bytes: &[u8], staged: &Path) -> AppResult<PathBuf> {
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
fn content_extension(format: &str) -> &str {
    match format {
        "jpeg" => "jpg",
        other => other,
    }
}

/// 写线程的待提交缓冲。`folder_ids` 跨批次保留，避免同一批导入反复解析同一目录。
#[derive(Default)]
struct WriteBuffer {
    prepared: Vec<Prepared>,
    duplicates: Vec<DuplicateOutcome>,
    folder_ids: HashMap<String, i64>,
}

fn write_loop(
    receiver: mpsc::Receiver<Outcome>,
    database: Arc<LibraryDb>,
    pack: Arc<ThumbPack>,
    options: WriterOptions,
    job: Arc<ImportJob>,
    index: Arc<RwLock<PhashIndex>>,
) {
    let mut buffer = WriteBuffer {
        prepared: Vec::with_capacity(BATCH_SIZE),
        ..Default::default()
    };

    loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(Outcome::Prepared(item)) => buffer.prepared.push(item),
            Ok(Outcome::Duplicate(item)) => buffer.duplicates.push(item),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                flush(&database, &pack, &options, &job, &index, &mut buffer);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                job.set_current(format!(
                    "正在写入最后一批（{} 张）",
                    buffer.prepared.len() + buffer.duplicates.len()
                ));
                flush(&database, &pack, &options, &job, &index, &mut buffer);
                break;
            }
        }

        if buffer.prepared.len() >= BATCH_SIZE || buffer.duplicates.len() >= DUPLICATE_BATCH {
            flush(&database, &pack, &options, &job, &index, &mut buffer);
        }
    }
}

fn flush(
    database: &LibraryDb,
    pack: &ThumbPack,
    options: &WriterOptions,
    job: &ImportJob,
    index: &RwLock<PhashIndex>,
    buffer: &mut WriteBuffer,
) {
    if buffer.prepared.is_empty() && buffer.duplicates.is_empty() {
        return;
    }
    let batch = std::mem::take(&mut buffer.prepared);
    let dups = std::mem::take(&mut buffer.duplicates);

    let commit = database.with_writer(|connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let mut created_tags = Vec::new();
        let tag_ids =
            repository::ensure_tags_tracked(&transaction, &options.tags, &mut created_tags)?;
        let mut created_folders: Vec<(i64, String)> = Vec::new();
        let mut merged: Vec<ImportMerged> = Vec::new();

        let mut stats = CommitStats {
            inserted: Vec::new(),
            failures: Vec::new(),
            duplicates: 0,
            merged: 0,
            temp_replaced: 0,
            optimized: 0,
            optimized_bytes: 0,
            written_bytes: 0,
            duplicate_pairs: Vec::new(),
            batch_duplicate_pairs: Vec::new(),
            merged_image_ids: Vec::new(),
            imported_pairs: Vec::new(),
            orphan_temp_files: Vec::new(),
        };
        let mut segments = HashSet::new();

        for item in &batch {
            let location = match pack.append(&item.thumb.bytes) {
                Ok(location) => location,
                Err(error) => {
                    stats.failures.push((
                        item.source_path.to_string_lossy().into_owned(),
                        error.to_string(),
                    ));
                    if item.published {
                        let _ = std::fs::remove_file(&item.destination);
                    }
                    continue;
                }
            };
            if segments.insert(location.segment) {
                repository::ensure_thumb_segment(&transaction, location.segment, now)?;
            }

            let insert = ImageInsert {
                content_hash: item.content_hash.clone(),
                file_ext: item.file_ext.clone(),
                file_size: item.file_size,
                width: item.width,
                height: item.height,
                format: item.format.clone(),
                has_alpha: item.has_alpha,
                phash: item.phash,
                colors4: item.colors4,
                colors8: item.colors8,
                score: options.score,
                thumb: location,
                thumb_format: "webp".into(),
                thumb_width: item.thumb.width,
                thumb_height: item.thumb.height,
                imported_at: now,
                optimized_at: item.optimized_at,
                origin_operation_id: options.operation_id,
            };

            let folder_id = match buffer.folder_ids.get(&item.folder) {
                Some(id) => *id,
                None => {
                    let id = if let Some(id) = options.folder_id {
                        id
                    } else {
                        repository::ensure_folder_tracked(
                            &transaction,
                            &item.folder,
                            now,
                            &mut created_folders,
                        )?
                    };
                    buffer.folder_ids.insert(item.folder.clone(), id);
                    id
                }
            };
            // 图库导入：同内容的临时行先让出唯一约束，否则这张图永远进不了图库。
            if options.replace_temp_duplicate
                && let Some(file_name) =
                    repository::discard_temporary_duplicate(&transaction, &item.content_hash)?
            {
                stats.temp_replaced += 1;
                if let Some(directory) = &options.temp_content_dir {
                    stats.orphan_temp_files.push(directory.join(file_name));
                }
            }
            match repository::insert_image(&transaction, folder_id, &insert) {
                Ok(Some(id)) => {
                    repository::attach_tags(&transaction, id, &tag_ids)?;
                    stats.inserted.push(InsertedRow {
                        id,
                        similar: item.similar,
                        phash: item.phash,
                    });
                    stats
                        .imported_pairs
                        .push((item.source_path.to_string_lossy().into_owned(), id));
                    // 压缩成功时 file_size 已是压缩结果，天然代表“最终新增体积”。
                    stats.written_bytes += item.file_size;
                    if let Some(saved) = item.optimized_savings {
                        stats.optimized += 1;
                        stats.optimized_bytes += saved;
                    }
                }
                Ok(None) => {
                    if let Some(found) = resolve_existing(
                        &transaction,
                        &item.content_hash,
                        options,
                        &tag_ids,
                        now,
                        &mut stats,
                        &mut merged,
                    )? {
                        stats.duplicate_pairs.push((
                            item.source_path.to_string_lossy().into_owned(),
                            found.id,
                            found.trashed,
                        ));
                    }
                    if item.published {
                        let _ = std::fs::remove_file(&item.destination);
                    }
                }
                Err(error) => {
                    stats.failures.push((
                        item.source_path.to_string_lossy().into_owned(),
                        error.to_string(),
                    ));
                    if item.published {
                        let _ = std::fs::remove_file(&item.destination);
                    }
                }
            }
        }

        for duplicate in &dups {
            let source_path = duplicate.source_path.to_string_lossy().into_owned();
            if duplicate.in_batch {
                // 批内重复的胜者已由工作线程的哈希占位决定，这里只计数与归因。
                stats.duplicates += 1;
                if let Some(kept_path) = &duplicate.kept_path {
                    stats
                        .batch_duplicate_pairs
                        .push((source_path, kept_path.to_string_lossy().into_owned()));
                }
                continue;
            }
            if options.policy == DuplicatePolicy::Skip {
                // 工作线程已经确认过库内这一行，跳过策略不需要在写事务里再查一次。
                stats.duplicates += 1;
                if let Some(existing_id) = duplicate.existing_id {
                    stats
                        .duplicate_pairs
                        .push((source_path, existing_id, duplicate.trashed));
                }
                continue;
            }
            let found = resolve_existing(
                &transaction,
                &duplicate.hash,
                options,
                &tag_ids,
                now,
                &mut stats,
                &mut merged,
            )?;
            if let Some(found) = found {
                stats
                    .duplicate_pairs
                    .push((source_path, found.id, found.trashed));
            }
        }

        for segment in segments {
            let Ok(size) = pack.segment_size_bytes(segment) else {
                continue;
            };
            if let Err(error) = repository::update_thumb_segment_size(&transaction, segment, size) {
                log::warn!("缩略图段 {segment} 尺寸记录失败：{error}");
            }
        }

        if let Some(operation_id) = options.operation_id {
            let affected = (stats.inserted.len() + stats.merged) as u64;
            // 只有真正写入内容才算开启新时间线：导入被冲突拒绝、或整批都是重复跳过时，
            // 灰色记录（可重做的撤销）必须保留。
            if affected > 0 || !created_folders.is_empty() || !created_tags.is_empty() {
                history::discard_redo_in_tx(&transaction)?;
            }
            let append = |record: ImportRecord| -> crate::app::error::AppResult<()> {
                history::append_live_in_tx(
                    &transaction,
                    operation_id,
                    history::ChunkFormat::Json,
                    &serde_json::to_vec(&record)?,
                )
            };
            if !created_folders.is_empty() {
                append(ImportRecord::Folders {
                    items: created_folders
                        .into_iter()
                        .map(|(id, path)| FolderRef { id, path })
                        .collect(),
                })?;
            }
            if !created_tags.is_empty() {
                append(ImportRecord::Tags {
                    items: created_tags
                        .into_iter()
                        .map(|(id, name)| ImportTag { id, name })
                        .collect(),
                })?;
            }
            if !merged.is_empty() {
                append(ImportRecord::Merged { items: merged })?;
            }
            if affected > 0 {
                history::add_live_affected_in_tx(&transaction, operation_id, affected)?;
            }
        }

        // 指针事务一旦提交就是持久的；先把本批追加的缩略图字节落盘，
        // 否则掉电后这批行会指向段尾之外。
        if let Err(error) = pack.sync() {
            log::warn!("缩略图段落盘失败：{error}");
        }
        transaction.commit()?;
        Ok(stats)
    });

    match commit {
        Ok(stats) => {
            job.inc_imported(stats.inserted.len());
            job.inc_written_bytes(stats.written_bytes);
            job.inc_duplicates(stats.duplicates);
            job.inc_merged(stats.merged);
            job.inc_temp_replaced(stats.temp_replaced);
            job.inc_optimized(stats.optimized, stats.optimized_bytes);
            // 行已经删掉，原图随临时区清空也会消失；这里只是提前省下磁盘。
            if !stats.orphan_temp_files.is_empty() {
                match options.resources.acquire_path_write(
                    &options.temp_folder,
                    &options.holder,
                    &|| job.should_cancel(),
                ) {
                    Ok(_guard) => {
                        for file in stats.orphan_temp_files {
                            if let Err(error) = std::fs::remove_file(&file) {
                                log::debug!(
                                    "清理临时区重复图失败：{error}；文件位于 {}",
                                    file.display()
                                );
                            }
                        }
                    }
                    Err(error) => {
                        log::debug!("放弃提前清理临时区重复图：{error:?}");
                    }
                }
            }
            for (source, kept) in stats.batch_duplicate_pairs {
                job.add_batch_duplicate_pair(source, kept);
            }
            for (source, existing_id, trashed) in stats.duplicate_pairs {
                job.add_duplicate_pair(source, existing_id, trashed);
            }
            for image_id in stats.merged_image_ids {
                job.add_merged_image(image_id);
            }
            for (source, image_id) in stats.imported_pairs {
                job.add_imported_pair(source, image_id);
            }
            {
                let mut hashes = index.write();
                for row in &stats.inserted {
                    job.add_preview(row.id);
                    hashes.push((row.id, phash::words(&row.phash)));
                    if let Some((similar_to, distance)) = row.similar {
                        job.add_hint(ImportHint {
                            image_id: row.id,
                            similar_to,
                            distance,
                        });
                        job.inc_similar(1);
                    }
                }
            }
            for (path, reason) in stats.failures {
                job.add_failure(path, reason);
                job.inc_failed();
            }
        }
        Err(error) => {
            log::error!("导入批次入库失败：{error}");
            for item in &batch {
                if item.published {
                    let _ = std::fs::remove_file(&item.destination);
                }
                job.add_failure(
                    item.source_path.to_string_lossy(),
                    format!("入库失败：{error}"),
                );
                job.inc_failed();
            }
            for duplicate in &dups {
                job.add_failure(
                    format!("content:{}", duplicate.hash),
                    format!("重复项处理失败：{error}"),
                );
                job.inc_failed();
            }
        }
    }
    job.inc_written(batch.len() + dups.len());
}

/// 内容哈希已存在时的统一出口：按策略合并元数据，否则计入重复跳过。
///
/// 合并要在写事务内按哈希重新取 id，这样即使工作线程与写线程之间隔了若干批次，
/// 也只会改到当前事务可见的那一行。返回命中的图片 id 与回收站状态，供回执归因。
fn resolve_existing(
    connection: &Connection,
    hash: &str,
    options: &WriterOptions,
    tag_ids: &[i64],
    now: i64,
    stats: &mut CommitStats,
    merged: &mut Vec<ImportMerged>,
) -> AppResult<Option<ResolvedExisting>> {
    let Some((id, trashed)) = repository::find_by_hash_state(connection, hash, true)? else {
        stats.duplicates += 1;
        return Ok(None);
    };
    if options.policy != DuplicatePolicy::Merge {
        stats.duplicates += 1;
        return Ok(Some(ResolvedExisting { id, trashed }));
    }
    let (old_score, old_modified_at): (i64, i64) = connection.query_row(
        "SELECT score, modified_at FROM images WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let old_tag_ids: Vec<i64> = {
        let mut statement = connection
            .prepare("SELECT tag_id FROM image_tags WHERE image_id = ?1 ORDER BY tag_id")?;
        statement
            .query_map([id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    repository::merge_metadata(connection, id, options.score, tag_ids, now)?;
    merged.push(ImportMerged {
        image_id: id,
        old_score,
        old_modified_at,
        old_tag_ids,
    });
    stats.merged += 1;
    stats.merged_image_ids.push(id);
    Ok(Some(ResolvedExisting { id, trashed }))
}

struct ResolvedExisting {
    id: i64,
    trashed: bool,
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use image::{DynamicImage, Rgb, RgbImage, Rgba, RgbaImage};
    use tempfile::tempdir;

    use super::*;
    use crate::{
        imageops::optimize::QualityTier,
        import::job::ImportSnapshot,
        library::{service, thumbs},
    };

    fn sample_image(seed: u32, width: u32, height: u32) -> DynamicImage {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let stripe = ((x + seed) / 17 + y / 23) % 3;
            *pixel = Rgb([
                ((x * 3 + seed * 11) % 256) as u8,
                ((y * 5 + seed * 7) % 256) as u8,
                (stripe * 90) as u8,
            ]);
        }
        DynamicImage::ImageRgb8(image)
    }

    fn write_samples(directory: &Path, count: u32, size: (u32, u32)) {
        std::fs::create_dir_all(directory).unwrap();
        for index in 0..count {
            let image = sample_image(index, size.0, size.1);
            let path = match index % 4 {
                0 => directory.join(format!("{index:04}.jpg")),
                1 => directory.join(format!("{index:04}.png")),
                2 => directory.join(format!("{index:04}.bmp")),
                _ => directory.join(format!("{index:04}.webp")),
            };
            if index % 4 == 3 {
                let (width, height) = (image.width(), image.height());
                let rgb = image.to_rgb8();
                let encoded = webp::Encoder::from_rgb(rgb.as_raw(), width, height).encode(75.0);
                std::fs::write(path, encoded.to_vec()).unwrap();
            } else {
                image.save(path).unwrap();
            }
        }
    }

    fn open_library(root: &Path) -> crate::library::service::LibrarySession {
        service::create(root, "导入测试", 2).unwrap().0
    }

    fn source(session: &crate::library::service::LibrarySession, root: &Path) -> ImportSource {
        ImportSource {
            database: session.db.clone(),
            thumbs: session.thumbs.clone(),
            root: root.to_path_buf(),
            operation_id: None,
            resources: Arc::new(crate::concurrency::resources::ResourceTable::new()),
            images: Arc::new(crate::concurrency::budget::ImageBudget::new(4)),
            memory: Arc::new(crate::concurrency::budget::ImageMemoryBudget::default()),
            holder: crate::concurrency::resources::HolderInfo::new("import-test", "导入"),
        }
    }

    fn request(directory: &Path) -> ImportRequest {
        ImportRequest {
            paths: vec![directory.to_string_lossy().into_owned()],
            target: ImportTarget::Library,
            folder: "导入".into(),
            folder_mode: FolderMode::Flat,
            tags: vec!["测试".into()],
            score: 3,
            duplicate_policy: DuplicatePolicy::Skip,
            perceptual_threshold: 6,
            optimize: None,
            #[cfg(test)]
            cancel_after: None,
        }
    }

    fn run_import(
        session: &crate::library::service::LibrarySession,
        root: &Path,
        dir: &Path,
    ) -> ImportJob {
        let job = Arc::new(ImportJob::new(
            "test-job",
            "测试样本",
            ImportTarget::Library,
        ));
        run(source(session, root), job.clone(), request(dir)).unwrap();
        job.finish(ImportPhase::Completed);
        Arc::try_unwrap(job).ok().unwrap()
    }

    /// 平滑渐变：PNG 转 JPEG 后能同时满足画质底线与 5% 体积差。
    fn smooth_image(seed: u32, width: u32, height: u32) -> DynamicImage {
        let mut image = RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let value = (x * 3 + y * 2 + seed) % 256;
            *pixel = Rgb([
                value as u8,
                ((value + 60) % 256) as u8,
                ((value + 120) % 256) as u8,
            ]);
        }
        DynamicImage::ImageRgb8(image)
    }

    fn optimized_request(
        directory: &Path,
        tier: QualityTier,
        output_format: OptimizeFormat,
    ) -> ImportRequest {
        ImportRequest {
            optimize: Some(crate::import::ImportOptimize {
                tier,
                output_format,
            }),
            ..request(directory)
        }
    }

    fn run_with(
        session: &crate::library::service::LibrarySession,
        root: &Path,
        request: ImportRequest,
    ) -> ImportSnapshot {
        let job = Arc::new(ImportJob::new(
            "optimized-job",
            "测试样本",
            ImportTarget::Library,
        ));
        run(source(session, root), job.clone(), request).unwrap();
        job.finish(ImportPhase::Completed);
        job.snapshot()
    }

    #[test]
    fn compresses_content_when_import_optimize_is_enabled() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let path = source_dir.join("smooth.png");
        smooth_image(7, 192, 128).save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let original_hash = blake3::hash(&original).to_hex().to_string();

        let library_root = temp.path().join("library");
        let session = open_library(&library_root);
        let snapshot = run_with(
            &session,
            &library_root,
            optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
        );

        assert_eq!(snapshot.progress.imported, 1, "{snapshot:?}");
        assert_eq!(snapshot.progress.optimized, 1, "{snapshot:?}");
        assert!(snapshot.progress.optimized_bytes > 0, "{snapshot:?}");
        // 新增体积取压缩后的最终字节，等于落盘文件大小。
        assert!(snapshot.progress.written_bytes > 0, "{snapshot:?}");

        let (hash, ext, format, size, optimized_at): (String, String, String, i64, i64) =
            session
                .db
                .with_reader(|connection| {
                    Ok(connection.query_row(
                    "SELECT content_hash, file_ext, format, file_size, optimized_at FROM images",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )?)
                })
                .unwrap();
        assert_ne!(hash, original_hash);
        assert_eq!(ext, "jpg");
        assert_eq!(format, "jpeg");
        assert!(optimized_at > 0);
        assert!(size < original.len() as i64);

        let content = library_root.join("导入").join(format!("{hash}.jpg"));
        assert!(content.is_file(), "缺少压缩后的内容文件");
        assert_eq!(std::fs::metadata(&content).unwrap().len(), size as u64);
        assert_eq!(snapshot.progress.written_bytes, size as u64);
        assert!(
            !library_root
                .join("导入")
                .join(format!("{original_hash}.png"))
                .exists()
        );
    }

    /// PNG 的 `acTL` 块表示这是 APNG；这里按块结构手工拼一份，CRC 必须算对，
    /// 否则解码器会先报错，测到的就不是压缩判据了。
    fn apng_bytes(image: &DynamicImage) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFF_u32;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    let mask = (crc & 1).wrapping_neg();
                    crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                }
            }
            !crc
        }

        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        // IHDR 紧跟在 8 字节签名后，长度固定 25 字节（长度 + 类型 + 13 数据 + CRC）。
        let ihdr_end = 8 + 25;
        let mut chunk = Vec::with_capacity(20);
        chunk.extend_from_slice(&8_u32.to_be_bytes());
        chunk.extend_from_slice(b"acTL");
        chunk.extend_from_slice(&2_u32.to_be_bytes());
        chunk.extend_from_slice(&0_u32.to_be_bytes());
        chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
        let mut out = png[..ihdr_end].to_vec();
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&png[ihdr_end..]);
        out
    }

    /// 动画 PNG 在导入压缩时必须保留原图：压成单帧会丢掉其余帧且不可逆。
    #[test]
    fn keeps_apng_source_when_import_optimize_is_enabled() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let bytes = apng_bytes(&smooth_image(11, 192, 128));
        let path = source_dir.join("animated.png");
        std::fs::write(&path, &bytes).unwrap();
        let original_hash = blake3::hash(&bytes).to_hex().to_string();

        let library_root = temp.path().join("library");
        let session = open_library(&library_root);
        let snapshot = run_with(
            &session,
            &library_root,
            optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
        );

        assert_eq!(snapshot.progress.imported, 1, "{snapshot:?}");
        assert_eq!(snapshot.progress.optimized, 0, "{snapshot:?}");
        // 判据不满足不是编码失败，不该进失败计数。
        assert_eq!(snapshot.progress.optimize_failed, 0, "{snapshot:?}");
        let (hash, ext, size, optimized_at): (String, String, i64, i64) = session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT content_hash, file_ext, file_size, optimized_at FROM images",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?)
            })
            .unwrap();
        assert_eq!(hash, original_hash, "入库的必须是原图字节");
        assert_eq!(ext, "png");
        assert_eq!(size, bytes.len() as i64);
        assert_eq!(optimized_at, 0);
    }

    #[test]
    fn keeps_transparent_source_when_output_is_jpeg() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let path = source_dir.join("alpha.png");
        let mut image = RgbaImage::new(96, 96);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgba([
                x as u8,
                y as u8,
                ((x + y) % 256) as u8,
                ((x * 2 + y * 3) % 256) as u8,
            ]);
        }
        DynamicImage::ImageRgba8(image).save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let original_hash = blake3::hash(&original).to_hex().to_string();

        let library_root = temp.path().join("library");
        let session = open_library(&library_root);
        let snapshot = run_with(
            &session,
            &library_root,
            optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
        );

        assert_eq!(snapshot.progress.imported, 1, "{snapshot:?}");
        assert_eq!(snapshot.progress.optimized, 0, "{snapshot:?}");
        let (hash, ext, optimized_at): (String, String, i64) = session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT content_hash, file_ext, optimized_at FROM images",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .unwrap();
        assert_eq!(hash, original_hash);
        assert_eq!(ext, "png");
        assert_eq!(optimized_at, 0);
        assert!(
            library_root
                .join("导入")
                .join(format!("{hash}.png"))
                .is_file()
        );
    }

    #[test]
    fn optimized_import_dedupes_same_params_and_splits_on_tier_change() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        smooth_image(3, 160, 120)
            .save(source_dir.join("smooth.png"))
            .unwrap();

        let library_root = temp.path().join("library");
        let session = open_library(&library_root);
        let first = run_with(
            &session,
            &library_root,
            optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
        );
        assert_eq!(first.progress.imported, 1, "{first:?}");

        let repeat = run_with(
            &session,
            &library_root,
            optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
        );
        assert_eq!(repeat.progress.imported, 0, "{repeat:?}");
        assert_eq!(repeat.progress.duplicates, 1, "{repeat:?}");

        let changed = run_with(
            &session,
            &library_root,
            optimized_request(&source_dir, QualityTier::Medium, OptimizeFormat::Jpeg),
        );
        assert_eq!(changed.progress.imported, 1, "{changed:?}");
        let rows: i64 = session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(rows, 2);
    }

    #[test]
    fn compression_keeps_thumbnail_phash_and_colors() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        smooth_image(11, 200, 140)
            .save(source_dir.join("smooth.png"))
            .unwrap();

        let plain_root = temp.path().join("plain");
        let plain = open_library(&plain_root);
        run_import(&plain, &plain_root, &source_dir);

        let optimized_root = temp.path().join("optimized");
        let optimized = open_library(&optimized_root);
        run_with(
            &optimized,
            &optimized_root,
            optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
        );

        let read_row = |session: &crate::library::service::LibrarySession| {
            session
                .db
                .with_reader(|connection| {
                    Ok(connection.query_row(
                        "SELECT phash, colors4, colors8 FROM images",
                        [],
                        |row| {
                            let phash: Vec<u8> = row.get(0)?;
                            let colors4: Vec<u8> = row.get(1)?;
                            let colors8: Vec<u8> = row.get(2)?;
                            Ok((phash, colors4, colors8))
                        },
                    )?)
                })
                .unwrap()
        };
        assert_eq!(read_row(&plain), read_row(&optimized));

        let plain_thumb = {
            let pointer = thumbs::load_pointer(&plain.db, 1).unwrap().unwrap();
            plain.thumbs.read(pointer.location).unwrap()
        };
        let optimized_thumb = {
            let pointer = thumbs::load_pointer(&optimized.db, 1).unwrap().unwrap();
            optimized.thumbs.read(pointer.location).unwrap()
        };
        assert_eq!(plain_thumb, optimized_thumb);
    }

    #[test]
    fn imports_mixed_formats_and_reports_duplicates() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 8, (120, 90));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        let job = run_import(&session, &library_root, &source_dir);
        let snapshot = job.snapshot();
        assert_eq!(snapshot.progress.imported, 8, "{snapshot:?}");
        assert_eq!(snapshot.progress.written, 8, "{snapshot:?}");
        assert_eq!(snapshot.progress.failed, 0, "{snapshot:?}");
        assert_eq!(snapshot.progress.total, 8);
        assert_eq!(snapshot.preview_ids.len(), 8);
        let unique: std::collections::HashSet<_> = snapshot.preview_ids.iter().collect();
        assert_eq!(unique.len(), 8, "同一文件被处理了不止一次");

        session
            .db
            .with_reader(|connection| {
                let count: i64 =
                    connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?;
                assert_eq!(count, 8);
                let missing: i64 = connection.query_row(
                    "SELECT count(*) FROM images
                     WHERE thumb_segment = 0 OR thumb_length = 0
                        OR thumb_format != 'webp'
                        OR length(phash) != 32 OR length(colors4) != 16
                        OR length(colors8) != 32",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(missing, 0);
                Ok(())
            })
            .unwrap();

        let pointers = session
            .db
            .with_reader(|connection| {
                let mut statement = connection
                    .prepare("SELECT id, content_hash, file_ext FROM images ORDER BY id")?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .unwrap();
        for (id, hash, ext) in &pointers {
            let pointer = thumbs::load_pointer(&session.db, *id).unwrap().unwrap();
            let bytes = session.thumbs.read(pointer.location).unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert!(decoded.width() > 0 && decoded.height() > 0);
            let content = library_root.join("导入").join(format!("{hash}.{ext}"));
            assert!(content.is_file(), "缺少原图 {}", content.display());
        }

        let second = Arc::new(ImportJob::new(
            "duplicate-job",
            "测试样本",
            ImportTarget::Library,
        ));
        run(
            source(&session, &library_root),
            second.clone(),
            request(&source_dir),
        )
        .unwrap();
        second.finish(ImportPhase::Completed);
        let snapshot = second.snapshot();
        assert_eq!(snapshot.progress.imported, 0);
        assert_eq!(snapshot.progress.duplicates, 8);
        assert_eq!(snapshot.progress.failed, 0);
    }

    #[test]
    fn preserves_source_folders_and_merges_same_names() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("素材A");
        let second = temp.path().join("素材B");
        std::fs::create_dir_all(first.join("人物").join("速写")).unwrap();
        std::fs::create_dir_all(second.join("人物").join("色彩")).unwrap();
        sample_image(1, 64, 48)
            .save(first.join("人物").join("速写").join("a.png"))
            .unwrap();
        sample_image(2, 64, 48)
            .save(second.join("人物").join("色彩").join("b.png"))
            .unwrap();
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        let mut preserve = request(&first);
        preserve.paths = vec![
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ];
        preserve.folder_mode = FolderMode::Preserve;
        let job = Arc::new(ImportJob::new(
            "preserve-job",
            "测试样本",
            ImportTarget::Library,
        ));
        run(source(&session, &library_root), job.clone(), preserve).unwrap();
        job.finish(ImportPhase::Completed);
        assert_eq!(job.snapshot().progress.imported, 2);

        let folders: Vec<String> = session
            .db
            .with_reader(|connection| {
                let mut statement = connection.prepare("SELECT path FROM folders ORDER BY path")?;
                let paths = statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok(paths)
            })
            .unwrap();
        assert!(folders.contains(&"导入/人物".to_owned()));
        assert!(folders.contains(&"导入/人物/速写".to_owned()));
        assert!(folders.contains(&"导入/人物/色彩".to_owned()));
        assert!(library_root.join("导入").join("人物").join("速写").is_dir());
        assert!(library_root.join("导入").join("人物").join("色彩").is_dir());
    }

    #[test]
    fn attributes_exact_duplicates_and_imported_pairs() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 2, (60, 60));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        let first = run_import(&session, &library_root, &source_dir).snapshot();
        assert_eq!(first.imported_pairs.len(), 2);
        assert!(first.duplicate_pairs.is_empty());
        let mut first_ids: Vec<i64> = first
            .imported_pairs
            .iter()
            .map(|pair| pair.image_id)
            .collect();
        first_ids.sort_unstable();

        let second = run_import(&session, &library_root, &source_dir).snapshot();
        assert_eq!(second.progress.duplicates, 2);
        assert_eq!(second.duplicate_pair_count, 2);
        assert_eq!(second.duplicate_pairs.len(), 2);
        assert!(!second.duplicate_pairs_truncated);
        let mut duplicate_ids: Vec<i64> = second
            .duplicate_pairs
            .iter()
            .map(|pair| pair.image_id)
            .collect();
        duplicate_ids.sort_unstable();
        assert_eq!(duplicate_ids, first_ids);
        assert!(second.duplicate_pairs.iter().all(|pair| !pair.trashed));
        assert!(second.imported_pairs.is_empty());
    }

    #[test]
    fn attributes_batch_duplicates_to_the_kept_source() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let bytes = std::fs::read({
            let path = source_dir.join("origin.png");
            sample_image(9, 48, 48).save(&path).unwrap();
            path
        })
        .unwrap();
        std::fs::write(source_dir.join("copy.png"), &bytes).unwrap();
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        let snapshot = run_import(&session, &library_root, &source_dir).snapshot();
        assert_eq!(snapshot.progress.imported, 1);
        assert_eq!(snapshot.progress.duplicates, 1);
        assert_eq!(snapshot.batch_duplicate_pair_count, 1);
        assert_eq!(snapshot.batch_duplicate_pairs.len(), 1);
        let pair = &snapshot.batch_duplicate_pairs[0];
        assert_ne!(pair.source_path, pair.kept_path);
        assert_eq!(snapshot.imported_pairs[0].source_path, pair.kept_path);
    }

    #[test]
    fn duplicate_pairs_mark_trashed_existing_images() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 1, (60, 60));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);
        let first = run_import(&session, &library_root, &source_dir).snapshot();
        let image_id = first.imported_pairs[0].image_id;
        session
            .db
            .with_writer(|connection| {
                connection.execute("UPDATE images SET deleted_at = 1 WHERE id = ?1", [image_id])?;
                Ok(())
            })
            .unwrap();

        let second = run_import(&session, &library_root, &source_dir).snapshot();
        assert_eq!(second.duplicate_pair_count, 1);
        assert_eq!(second.duplicate_pairs[0].image_id, image_id);
        assert!(second.duplicate_pairs[0].trashed);
    }

    #[test]
    fn merges_metadata_for_existing_content() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 1, (60, 60));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);
        run_import(&session, &library_root, &source_dir);

        let mut merge_request = request(&source_dir);
        merge_request.duplicate_policy = DuplicatePolicy::Merge;
        merge_request.tags = vec!["第二次".into()];
        merge_request.score = 5;
        let job = Arc::new(ImportJob::new(
            "merge-job",
            "测试样本",
            ImportTarget::Library,
        ));
        run(source(&session, &library_root), job.clone(), merge_request).unwrap();
        job.finish(ImportPhase::Completed);
        let snapshot = job.snapshot();
        assert_eq!(snapshot.progress.merged, 1);
        assert_eq!(snapshot.progress.duplicates, 0);
        let image_id: i64 = session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row("SELECT id FROM images", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(snapshot.merged_image_ids, vec![image_id]);
        assert_eq!(snapshot.duplicate_pair_count, 1);
        assert_eq!(snapshot.duplicate_pairs[0].image_id, image_id);

        session
            .db
            .with_reader(|connection| {
                let (score, tags): (i64, i64) = connection.query_row(
                    "SELECT score,
                            (SELECT count(*) FROM image_tags WHERE image_id = images.id)
                     FROM images",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(score, 5);
                assert_eq!(tags, 2);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn library_import_replaces_same_content_in_temp_folder() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 3, (40, 40));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        // 先进临时区：路径不变，源文件也还在，正好模拟“先看一眼再决定入库”。
        let mut temp_request = request(&source_dir);
        temp_request.target = ImportTarget::Temp;
        temp_request.folder.clear();
        temp_request.tags.clear();
        let temp_snapshot = run_with(&session, &library_root, temp_request);
        assert_eq!(temp_snapshot.progress.imported, 3, "{temp_snapshot:?}");

        // 同一个目录再导图库：临时区的同内容行让位，三张都真正入库。
        let snapshot = run_with(&session, &library_root, request(&source_dir));
        assert_eq!(snapshot.progress.imported, 3, "{snapshot:?}");
        assert_eq!(snapshot.progress.duplicates, 0, "{snapshot:?}");
        assert_eq!(snapshot.progress.temp_replaced, 3, "{snapshot:?}");

        let (library_rows, temp_rows): (i64, i64) = session
            .db
            .with_reader(|connection| {
                Ok((
                    connection.query_row(
                        "SELECT count(*) FROM images JOIN folders ON folders.id = images.folder_id
                         WHERE folders.kind = 'normal'",
                        [],
                        |row| row.get(0),
                    )?,
                    connection.query_row(
                        "SELECT count(*) FROM images JOIN folders ON folders.id = images.folder_id
                         WHERE folders.kind = 'temp'",
                        [],
                        |row| row.get(0),
                    )?,
                ))
            })
            .unwrap();
        assert_eq!((library_rows, temp_rows), (3, 0));

        // 临时区留下的原图副本在提交后被清掉，不占着磁盘等关库。
        let temp_dir = library_root.join(crate::library::paths::TEMP_FOLDER_DIR);
        let leftover = std::fs::read_dir(&temp_dir).unwrap().count();
        assert_eq!(leftover, 0, "临时区的原图副本没有清理");
    }

    #[test]
    fn cancellation_keeps_committed_rows_and_cleans_temp() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 120, (48, 48));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        let mut cancel_request = request(&source_dir);
        cancel_request.cancel_after = Some(20);
        let job = Arc::new(ImportJob::new(
            "cancel-job",
            "测试样本",
            ImportTarget::Library,
        ));
        run(source(&session, &library_root), job.clone(), cancel_request).unwrap();
        assert!(job.should_cancel());
        job.finish(ImportPhase::Cancelled);
        let snapshot = job.snapshot();
        assert!(snapshot.progress.processed < snapshot.progress.total);
        assert_eq!(
            snapshot.progress.written, snapshot.progress.imported,
            "已入库数量必须与 written 一致：{snapshot:?}"
        );
        assert!(snapshot.progress.written <= snapshot.progress.processed);

        let rows: i64 = session
            .db
            .with_reader(|connection| {
                let rows =
                    connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(rows as usize, snapshot.progress.imported);

        let temp_dir = library_root.join(".artbank").join("temp");
        let leftover: Vec<_> = std::fs::read_dir(&temp_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(leftover.is_empty(), "临时目录未清理：{leftover:?}");
    }

    #[test]
    fn missing_pack_is_reported_not_panicking() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 1, (50, 50));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);
        run_import(&session, &library_root, &source_dir);

        std::fs::remove_file(
            library_root
                .join(".artbank")
                .join("thumbs")
                .join("0001.pack"),
        )
        .unwrap();
        let pointer = thumbs::load_pointer(&session.db, 1).unwrap().unwrap();
        assert!(session.thumbs.read(pointer.location).is_err());
    }

    /// 发布时目标已存在：内容一致才接管，截断的孤儿必须被替换。
    #[test]
    fn publish_adopts_matching_orphan_and_replaces_truncated_one() {
        let temp = tempdir().unwrap();
        let mut payload = Vec::new();
        smooth_image(5, 64, 48)
            .write_to(
                &mut std::io::Cursor::new(&mut payload),
                image::ImageFormat::Png,
            )
            .unwrap();
        let hash = blake3::hash(&payload).to_hex().to_string();
        let destination = temp.path().join(format!("{hash}.png"));

        // 完整孤儿：接管，返回 false 表示目标文件不是本批写的。
        std::fs::write(&destination, &payload).unwrap();
        let staged = temp.path().join("staged-1.png");
        std::fs::write(&staged, &payload).unwrap();
        assert!(!publish(&staged, &destination, &hash).unwrap());
        assert!(!staged.exists(), "接管后临时文件应删除");
        assert_eq!(std::fs::read(&destination).unwrap(), payload);

        // 截断孤儿：内容不匹配，删掉后重新发布。
        std::fs::write(&destination, &payload[..payload.len() / 2]).unwrap();
        let staged = temp.path().join("staged-2.png");
        std::fs::write(&staged, &payload).unwrap();
        assert!(publish(&staged, &destination, &hash).unwrap());
        assert!(!staged.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), payload);
    }

    /// 目标位置已有截断的孤儿时，导入不能直接把它登记成正常图片。
    #[test]
    fn import_replaces_truncated_orphan_instead_of_registering_it() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let path = source_dir.join("photo.png");
        smooth_image(9, 96, 64).save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let hash = blake3::hash(&original).to_hex().to_string();

        let library_root = temp.path().join("library");
        let session = open_library(&library_root);
        let destination =
            LibraryPaths::new(&library_root).content_file("导入", &format!("{hash}.png"));
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(&destination, &original[..original.len() / 2]).unwrap();

        let snapshot = run_with(&session, &library_root, request(&source_dir));
        assert_eq!(snapshot.progress.imported, 1, "{snapshot:?}");
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            original,
            "截断的孤儿应被完整内容替换"
        );
    }

    #[test]
    #[ignore = "高成本基准（非日常）：生成并导入 1000 张样本；用 cargo test -- --ignored --nocapture perf_import_1000 运行"]
    fn perf_import_1000() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 1000, (768, 512));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        let job = Arc::new(ImportJob::new(
            "perf-job",
            "测试样本",
            ImportTarget::Library,
        ));
        let start = Instant::now();
        run(
            source(&session, &library_root),
            job.clone(),
            request(&source_dir),
        )
        .unwrap();
        let elapsed = start.elapsed();
        job.finish(ImportPhase::Completed);
        let snapshot = job.snapshot();
        let seconds = elapsed.as_secs_f64();
        let rate = snapshot.progress.imported as f64 / seconds;

        println!(
            "[perf] import: imported={} failed={} duplicates={} total={} elapsed={:.2}s rate={:.1}/s",
            snapshot.progress.imported,
            snapshot.progress.failed,
            snapshot.progress.duplicates,
            snapshot.progress.total,
            seconds,
            rate
        );

        assert_eq!(snapshot.progress.imported, 1000);
        assert_eq!(snapshot.progress.failed, 0);
        assert!(rate >= 20.0, "导入吞吐 {rate:.1}/s 低于 20/s");
    }

    #[test]
    fn writer_guard_joins_when_the_worker_unwinds() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_flag = finished.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = WriterGuard::new(std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(120));
                worker_flag.store(true, Ordering::SeqCst);
            }));
            panic!("工作线程 panic");
        }));
        assert!(outcome.is_err());
        assert!(
            finished.load(Ordering::SeqCst),
            "写线程必须在外层 panic 展开时被 join 掉"
        );
    }
}

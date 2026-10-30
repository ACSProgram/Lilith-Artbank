use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
};

use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;

use crate::{
    app::error::{AppError, AppResult},
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget},
        resources::{HolderInfo, ResourceTable},
    },
    imageops::{optimize::OptimizeOptions, phash, thumbnail},
    import::{
        FolderMode, ImportRequest, ImportSource, ImportTarget,
        job::{DuplicatePolicy, ImportJob, ImportPhase},
        scan,
    },
    library::{db::LibraryDb, import as repository, paths::LibraryPaths},
};

mod publish;
mod scan_job;
#[cfg(test)]
mod tests;
mod writer;

use scan_job::process_one;
use writer::{WriterGuard, write_loop};

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

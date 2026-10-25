use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::Duration,
};

use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use rusqlite::Connection;
use uuid::Uuid;

use crate::{
    app::error::{AppError, AppResult},
    imageops::{colors, phash, thumbnail},
    import::{
        FolderMode, ImportRequest, ImportSource, decode,
        job::{DuplicatePolicy, ImportHint, ImportJob, ImportPhase},
        scan,
    },
    library::{
        db::LibraryDb,
        history::{self, FolderRef, ImportMerged, ImportRecord, ImportTag},
        import as repository,
        import::ImageInsert,
        paths::{LibraryPaths, join_folder, normalize_folder},
        service::now_ms,
        thumbs::ThumbPack,
    },
};

const BATCH_SIZE: usize = 64;
const DUPLICATE_BATCH: usize = 128;
const CHANNEL_DEPTH: usize = 256;
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
}

/// 工作线程交给写线程的结果。`Prepared` 是热点载荷，装箱会为每张图多一次分配，
/// 因此保留内联；通道深度 256，峰值占用约 75 KB。
#[allow(clippy::large_enum_variant)]
enum Outcome {
    Prepared(Prepared),
    Duplicate { hash: String, in_batch: bool },
}

struct DuplicateOutcome {
    hash: String,
    in_batch: bool,
}

struct WorkerContext {
    job: Arc<ImportJob>,
    database: Arc<LibraryDb>,
    paths: LibraryPaths,
    temp_dir: PathBuf,
    folder: String,
    folder_mode: FolderMode,
    threshold: u8,
    inflight: Arc<Mutex<HashSet<String>>>,
    index: Arc<RwLock<PhashIndex>>,
    sender: mpsc::SyncSender<Outcome>,
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

    let inflight = Arc::new(Mutex::new(HashSet::new()));
    let (sender, receiver) = mpsc::sync_channel::<Outcome>(CHANNEL_DEPTH);
    let writer_options = WriterOptions {
        tags: request.tags.clone(),
        score: request.score,
        policy: request.duplicate_policy,
        operation_id: source.operation_id,
    };

    let writer = {
        let database = source.database.clone();
        let pack = source.thumbs.clone();
        let job = job.clone();
        let index = index.clone();
        std::thread::spawn(move || {
            write_loop(receiver, database, pack, writer_options, job, index);
        })
    };

    let threads = request
        .worker_threads
        .filter(|threads| (1..=128).contains(threads))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|count| count.get().saturating_sub(1))
                .unwrap_or(1)
                .max(1)
        });
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("import-{index}"))
        .build()
        .map_err(|error| AppError::message(format!("无法创建导入工作池：{error}")))?;

    {
        let context = WorkerContext {
            job: job.clone(),
            database: source.database.clone(),
            paths: paths.clone(),
            temp_dir: temp_dir.to_path_buf(),
            folder: request.folder.clone(),
            folder_mode: request.folder_mode,
            threshold: request.perceptual_threshold,
            inflight,
            index,
            sender,
            #[cfg(test)]
            cancel_after: request.cancel_after,
        };
        pool.install(|| {
            scanned
                .files
                .par_iter()
                .for_each(|file| process_one(&file.path, &file.relative_folder, &context));
        });
    }

    job.set_phase(ImportPhase::Finishing);
    match writer.join() {
        Ok(()) => Ok(()),
        Err(_) => {
            job.add_failure("", "入库线程异常终止，部分结果可能未写入");
            job.inc_failed();
            Ok(())
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

    match process_file(path, relative_folder, context) {
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

    let existing = context
        .database
        .with_reader(|connection| repository::find_by_hash(connection, &staged.hash))?;
    if existing.is_some() {
        return Ok(Some(Outcome::Duplicate {
            hash: staged.hash.clone(),
            in_batch: false,
        }));
    }

    {
        let mut inflight = context.inflight.lock();
        if !inflight.insert(staged.hash.clone()) {
            return Ok(Some(Outcome::Duplicate {
                hash: staged.hash.clone(),
                in_batch: true,
            }));
        }
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

    let decoded = decode::decode(&staged.path)?;
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
    let file_ext = content_extension(decoded.format).to_owned();
    let file_name = format!("{}.{}", staged.hash, file_ext);
    let destination = context.paths.content_file(&folder, &file_name);
    let published = publish(&staged.path, &destination)?;

    Ok(Some(Outcome::Prepared(Prepared {
        source_path: source_path.to_path_buf(),
        destination,
        folder,
        published,
        content_hash: staged.hash.clone(),
        file_ext,
        file_size: staged.size,
        width: decoded.width,
        height: decoded.height,
        format: decoded.format.to_owned(),
        has_alpha: decoded.has_alpha,
        phash: hash,
        colors4,
        colors8,
        thumb,
        similar,
    })))
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
        Err(error) => Err(AppError::message(format!("发布原图失败：{error}"))),
    }
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
            Ok(Outcome::Duplicate { hash, in_batch }) => {
                buffer.duplicates.push(DuplicateOutcome { hash, in_batch })
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                flush(&database, &pack, &options, &job, &index, &mut buffer);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
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
                origin_operation_id: options.operation_id,
            };

            let folder_id = match buffer.folder_ids.get(&item.folder) {
                Some(id) => *id,
                None => {
                    let id = repository::ensure_folder_tracked(
                        &transaction,
                        &item.folder,
                        now,
                        &mut created_folders,
                    )?;
                    buffer.folder_ids.insert(item.folder.clone(), id);
                    id
                }
            };
            match repository::insert_image(&transaction, folder_id, &insert) {
                Ok(Some(id)) => {
                    repository::attach_tags(&transaction, id, &tag_ids)?;
                    stats.inserted.push(InsertedRow {
                        id,
                        similar: item.similar,
                        phash: item.phash,
                    });
                }
                Ok(None) => {
                    resolve_existing(
                        &transaction,
                        &item.content_hash,
                        options,
                        &tag_ids,
                        now,
                        &mut stats,
                        &mut merged,
                    )?;
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
            if duplicate.in_batch {
                // 批内重复的胜者已由工作线程的 HashSet 占位决定，这里只计数。
                stats.duplicates += 1;
                continue;
            }
            resolve_existing(
                &transaction,
                &duplicate.hash,
                options,
                &tag_ids,
                now,
                &mut stats,
                &mut merged,
            )?;
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
            let affected = (stats.inserted.len() + stats.merged) as u64;
            if affected > 0 {
                history::add_live_affected_in_tx(&transaction, operation_id, affected)?;
            }
        }

        transaction.commit()?;
        Ok(stats)
    });

    match commit {
        Ok(stats) => {
            job.inc_imported(stats.inserted.len());
            job.inc_duplicates(stats.duplicates);
            job.inc_merged(stats.merged);
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
}

/// 内容哈希已存在时的统一出口：按策略合并元数据，否则计入重复跳过。
///
/// 合并要在写事务内按哈希重新取 id，这样即使工作线程与写线程之间隔了若干批次，
/// 也只会改到当前事务可见的那一行。
fn resolve_existing(
    connection: &Connection,
    hash: &str,
    options: &WriterOptions,
    tag_ids: &[i64],
    now: i64,
    stats: &mut CommitStats,
    merged: &mut Vec<ImportMerged>,
) -> AppResult<()> {
    match repository::find_by_hash(connection, hash)? {
        Some(id) if options.policy == DuplicatePolicy::Merge => {
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
        }
        _ => stats.duplicates += 1,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use image::{DynamicImage, Rgb, RgbImage};
    use tempfile::tempdir;

    use super::*;
    use crate::library::{service, thumbs};

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
        }
    }

    fn request(directory: &Path) -> ImportRequest {
        ImportRequest {
            paths: vec![directory.to_string_lossy().into_owned()],
            folder: "导入".into(),
            folder_mode: FolderMode::Flat,
            tags: vec!["测试".into()],
            score: 3,
            duplicate_policy: DuplicatePolicy::Skip,
            perceptual_threshold: 6,
            worker_threads: Some(4),
            #[cfg(test)]
            cancel_after: None,
        }
    }

    fn run_import(
        session: &crate::library::service::LibrarySession,
        root: &Path,
        dir: &Path,
    ) -> ImportJob {
        let job = Arc::new(ImportJob::new("test-job", "测试样本"));
        run(source(session, root), job.clone(), request(dir)).unwrap();
        job.finish(ImportPhase::Completed);
        Arc::try_unwrap(job).ok().unwrap()
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
        assert_eq!(snapshot.progress.failed, 0, "{snapshot:?}");
        assert_eq!(snapshot.progress.total, 8);
        assert_eq!(snapshot.preview_ids.len(), 8);

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

        let second = Arc::new(ImportJob::new("duplicate-job", "测试样本"));
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
        let job = Arc::new(ImportJob::new("preserve-job", "测试样本"));
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
        let job = Arc::new(ImportJob::new("merge-job", "测试样本"));
        run(source(&session, &library_root), job.clone(), merge_request).unwrap();
        job.finish(ImportPhase::Completed);
        let snapshot = job.snapshot();
        assert_eq!(snapshot.progress.merged, 1);
        assert_eq!(snapshot.progress.duplicates, 0);

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
    fn cancellation_keeps_committed_rows_and_cleans_temp() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 120, (48, 48));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        let mut cancel_request = request(&source_dir);
        cancel_request.worker_threads = Some(4);
        cancel_request.cancel_after = Some(20);
        let job = Arc::new(ImportJob::new("cancel-job", "测试样本"));
        run(source(&session, &library_root), job.clone(), cancel_request).unwrap();
        assert!(job.should_cancel());
        job.finish(ImportPhase::Cancelled);
        let snapshot = job.snapshot();
        assert!(snapshot.progress.processed < snapshot.progress.total);

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

    #[test]
    #[ignore = "生成并导入 1000 张样本；用 cargo test -- --ignored --nocapture perf_import_1000 运行"]
    fn perf_import_1000() {
        let temp = tempdir().unwrap();
        let source_dir = temp.path().join("source");
        write_samples(&source_dir, 1000, (768, 512));
        let library_root = temp.path().join("library");
        let session = open_library(&library_root);

        let job = Arc::new(ImportJob::new("perf-job", "测试样本"));
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
}

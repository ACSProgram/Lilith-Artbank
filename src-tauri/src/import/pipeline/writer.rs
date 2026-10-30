use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, mpsc},
    thread::JoinHandle,
    time::Duration,
};

use parking_lot::RwLock;
use rusqlite::Connection;

use crate::{
    app::error::AppResult,
    imageops::phash,
    import::job::{DuplicatePolicy, ImportHint, ImportJob},
    library::{
        db::LibraryDb,
        history::{self, FolderRef, ImportMerged, ImportRecord, ImportTag},
        import as repository,
        import::ImageInsert,
        service::now_ms,
        thumbs::ThumbPack,
    },
};

use super::{
    BATCH_SIZE, CommitStats, DUPLICATE_BATCH, DuplicateOutcome, InsertedRow, Outcome, PhashIndex,
    Prepared, WriterOptions,
};

/// 入库线程的 join 守卫。
///
/// 工作线程 panic 会在 `pool.install` 处展开，正常路径的 `join` 就执行不到了；
/// 写线程仍持有通道接收端，会继续把已排队的结果写进库，甚至在本任务收尾之后还在写。
/// 交给 Drop 兜底，保证任何退出路径都先排空写线程，再让调用方 `finish`。
/// 写线程只在 sender 全部释放后退出，所以 `WriterGuard` 必须声明在 `context`（持有
/// sender）之前：drop 时 context 先释放，最后才轮到 guard 的 join。
pub(super) struct WriterGuard(Option<JoinHandle<()>>);

impl WriterGuard {
    pub(super) fn new(handle: JoinHandle<()>) -> Self {
        Self(Some(handle))
    }

    /// 显式等待；重复调用直接返回成功。
    pub(super) fn join(&mut self) -> std::thread::Result<()> {
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
/// 写线程的待提交缓冲。`folder_ids` 跨批次保留，避免同一批导入反复解析同一目录。
#[derive(Default)]
struct WriteBuffer {
    prepared: Vec<Prepared>,
    duplicates: Vec<DuplicateOutcome>,
    folder_ids: HashMap<String, i64>,
}

pub(super) fn write_loop(
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

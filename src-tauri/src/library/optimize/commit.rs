use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Instant,
};

use rayon::prelude::*;
use rusqlite::params;

use crate::{
    app::error::{AppError, AppResult},
    library::{
        content,
        db::LibraryDb,
        import as repository,
        metadata::{self, ContentRemover},
        paths::LibraryPaths,
        service::now_ms,
        thumbs::ThumbPack,
        viewer,
    },
    platform::fs as durable,
};

use super::{
    OptimizeReport,
    execute::{EncodedItem, discard_written, referenced_hashes},
};

pub(super) struct Published {
    pub(super) row_id: i64,
    pub(super) old_hash: String,
    pub(super) old_path: PathBuf,
    pub(super) old_size: u64,
    pub(super) new_size: u64,
}

#[derive(Default)]
pub(super) struct CommitOutcome {
    pub(super) published: Vec<Published>,
    pub(super) duplicates: u64,
    pub(super) write_ms: u64,
    pub(super) commit_ms: u64,
}

/// 发布一批：先把文件持久化到目标内容路径并 flush 目录，再在一个 FULL 事务里更新行。
///
/// 返回已提交的条目与因目标内容已被占用而跳过的张数。提交失败时删掉本批已发布的
/// 新文件——事务已回滚，没有行指向它们。
pub(super) fn commit_batch(
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

pub(super) enum PublishOutcome {
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
pub(super) fn publish(
    temp: &Path,
    destination: &Path,
    expected_hash: &str,
) -> AppResult<PublishOutcome> {
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
pub(super) fn delete_replaced(
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

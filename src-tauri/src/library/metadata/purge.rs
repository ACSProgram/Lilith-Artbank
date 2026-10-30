use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use rusqlite::{params_from_iter, types::Value};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        BatchReporter,
        db::LibraryDb,
        history::{self, HistoryKind, NewOperation},
        paths::{
            LibraryPaths, ensure_canonical_within, ensure_within, normalize_content_extension,
            normalize_stored_folder, subtree_like_pattern,
        },
        placeholders,
    },
};

use super::{
    SQL_CHUNK,
    r#move::{BatchFailure, DeleteReport},
};

/// 永久删除/清空回收站只写待释放标记；文件在对应记录过期或清空记录后由释放任务删除。
pub(super) fn mark_for_purge(
    database: &LibraryDb,
    image_ids: Option<&[i64]>,
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    let targets = purge_targets(database, image_ids)?;
    let folder_targets = if image_ids.is_none() {
        database.with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM folders
                 WHERE kind = 'normal' AND trashed_at IS NOT NULL
                   AND (purge_operation_id IS NULL
                        OR purge_operation_id NOT IN (SELECT id FROM operations))",
                [],
                |row| row.get::<_, i64>(0),
            )? as u64)
        })?
    } else {
        0
    };
    let (kind, label) = if image_ids.is_some() {
        (HistoryKind::PermanentDelete, "永久删除")
    } else {
        (HistoryKind::EmptyTrash, "清空回收站")
    };
    if targets.is_empty() && folder_targets == 0 {
        return Ok(DeleteReport {
            deferred: true,
            ..Default::default()
        });
    }
    let total = targets.len();
    reporter.report(0, total);

    history::write(
        database,
        NewOperation::new(kind, label),
        |transaction, draft| {
            let mut changed = 0_u64;
            let mut done = 0_usize;
            for chunk in targets.chunks(SQL_CHUNK) {
                if reporter.cancelled() {
                    break;
                }
                let marks = placeholders(chunk.len());
                changed += transaction.execute(
                    &format!(
                        "UPDATE images SET purge_operation_id = ?1
                     WHERE id IN ({marks}) AND deleted_at IS NOT NULL
                       AND purge_operation_id IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(draft.id()))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )? as u64;
                done += chunk.len();
                reporter.report(done, total);
            }
            let folders_marked = if image_ids.is_none() {
                // 无主标记（记录已被裁剪或清空）一并改成这次的记录：上次因为目录不空
                // 没释放掉的文件夹，下次清空回收站时再试一次。
                transaction.execute(
                    "UPDATE folders SET purge_operation_id = ?1
                     WHERE kind = 'normal' AND trashed_at IS NOT NULL
                       AND (purge_operation_id IS NULL
                            OR purge_operation_id NOT IN (SELECT id FROM operations))",
                    [draft.id()],
                )? as u64
            } else {
                0
            };
            draft.affected(changed + folders_marked);
            Ok(DeleteReport {
                deleted: changed,
                files_removed: 0,
                deferred: true,
                failures: Vec::new(),
            })
        },
    )
}

/// 永久删除要求整批都在回收站（否则拒绝，避免绕过界面隐藏正常图片）；清空回收站取全部可删行。
fn purge_targets(database: &LibraryDb, image_ids: Option<&[i64]>) -> AppResult<Vec<i64>> {
    let Some(ids) = image_ids else {
        return database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id FROM images
                 WHERE deleted_at IS NOT NULL AND purge_operation_id IS NULL",
            )?;
            let ids = statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        });
    };
    let mut eligible = Vec::with_capacity(ids.len());
    database.with_reader(|connection| {
        for chunk in ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let mut statement = connection.prepare(&format!(
                "SELECT id FROM images
                 WHERE id IN ({marks}) AND deleted_at IS NOT NULL
                   AND purge_operation_id IS NULL"
            ))?;
            eligible.extend(
                statement
                    .query_map(params_from_iter(chunk.iter()), |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(())
    })?;
    if eligible.len() != ids.len() {
        return Err(AppError::message("只能永久删除回收站中的图片"));
    }
    let blocked = database.with_reader(|connection| {
        for chunk in ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let count: i64 = connection.query_row(
                &format!(
                    "SELECT count(*) FROM images
                     JOIN folders ON folders.id = images.folder_id
                     WHERE images.id IN ({marks}) AND folders.trashed_at IS NOT NULL"
                ),
                params_from_iter(chunk.iter()),
                |row| row.get(0),
            )?;
            if count > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    })?;
    if blocked {
        return Err(AppError::message(
            "已删除文件夹下的图片不能单张永久删除；请永久删除整个文件夹",
        ));
    }
    Ok(eligible)
}

/// 释放任务：按 `purge_operation_id` 删行并删原图；每块先提交删行，再删文件。最后清理回收站文件夹行与空目录。
pub fn purge_by_operation(
    database: &LibraryDb,
    root: &Path,
    operation_ids: &[i64],
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    if operation_ids.is_empty() {
        return Err(AppError::message("没有指定待释放的操作记录"));
    }
    let paths = LibraryPaths::new(root);
    let selected = database.with_reader(|connection| {
        let mut selected = Vec::new();
        for chunk in operation_ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let mut statement = connection.prepare(&format!(
                "SELECT images.id, images.content_hash, images.file_ext, folders.path
                 FROM images JOIN folders ON folders.id = images.folder_id
                 WHERE images.purge_operation_id IN ({marks})"
            ))?;
            selected.extend(
                statement
                    .query_map(params_from_iter(chunk.iter()), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(selected)
    })?;

    let mut files = Vec::with_capacity(selected.len());
    // 路径无效的行不能删：删了用户既看不见也查不到，留着还能在检查图库里定位。
    let mut invalid: Vec<(i64, String, String)> = Vec::new();
    for (image_id, content_hash, file_ext, folder_path) in selected {
        let stored = format!("{folder_path}/{content_hash}.{file_ext}");
        match content_path(&paths, &folder_path, &content_hash, &file_ext) {
            Ok(path) => files.push((image_id, path)),
            Err(error) => {
                log::warn!("保留无法定位原图的图片行 image={image_id}，原路径 {stored}：{error}");
                invalid.push((image_id, stored, error.to_string()));
            }
        }
    }

    let total = files.len() + invalid.len();
    reporter.report(0, total);
    let mut report = DeleteReport::default();
    let mut done = 0_usize;

    // 无效路径行只清掉待释放标记：留着标记会让「还有待释放文件」永远为真，
    // 每次写入后都重新排一次释放任务，而且这个任务永远清不掉它。
    for chunk in invalid.chunks(SQL_CHUNK) {
        let ids: Vec<i64> = chunk.iter().map(|(id, _, _)| *id).collect();
        database.with_durable_writer(|connection| {
            let transaction = connection.transaction()?;
            let marks = placeholders(ids.len());
            let ops = placeholders(operation_ids.len());
            transaction.execute(
                &format!(
                    "UPDATE images SET purge_operation_id = NULL
                     WHERE id IN ({marks}) AND purge_operation_id IN ({ops})"
                ),
                params_from_iter(ids.iter().chain(operation_ids.iter())),
            )?;
            transaction.commit()?;
            Ok(())
        })?;
        for (image_id, stored, reason) in chunk {
            report.failures.push(BatchFailure {
                image_id: Some(*image_id),
                path: stored.clone(),
                reason: format!("无法定位原图，已保留图片行：{reason}"),
            });
            done += 1;
            reporter.report(done, total);
        }
    }

    for chunk in files.chunks(SQL_CHUNK) {
        if reporter.cancelled() {
            break;
        }
        let ids: Vec<i64> = chunk.iter().map(|(id, _)| *id).collect();
        // 删行提交必须先落盘，之后才允许删文件；否则断电后行还在、原图已没了。
        let deleted = database.with_durable_writer(|connection| {
            let transaction = connection.transaction()?;
            let marks = placeholders(ids.len());
            let deleted = transaction.execute(
                &format!("DELETE FROM images WHERE id IN ({marks})"),
                params_from_iter(ids.iter()),
            )?;
            transaction.commit()?;
            Ok(deleted as u64)
        })?;
        report.deleted += deleted;

        for (image_id, path) in chunk {
            if path.exists() {
                match remove_content_file(&paths.root, path) {
                    Ok(()) => report.files_removed += 1,
                    Err(error) => {
                        log::warn!(
                            "删除原图失败 image={image_id}，文件位于 {}：{error}",
                            path.display()
                        );
                        report.failures.push(BatchFailure {
                            image_id: Some(*image_id),
                            path: path.to_string_lossy().into_owned(),
                            reason: error.to_string(),
                        });
                    }
                }
            }
            done += 1;
            reporter.report(done, total);
        }
    }
    if !reporter.cancelled() {
        purge_folders_for_operation(database, root, operation_ids, &mut report)?;
    }
    Ok(report)
}

/// 释放文件夹：自底向上删除已无图片、已无子文件夹且磁盘目录为空的回收站文件夹行。
///
/// 目录里还有未纳管文件时保留行与待释放标记，只记日志与失败清单；不会递归强删。
/// 这类文件夹不在每次开库时自动重试，等下次清空回收站重新标记时再试。
fn purge_folders_for_operation(
    database: &LibraryDb,
    root: &Path,
    operation_ids: &[i64],
    report: &mut DeleteReport,
) -> AppResult<()> {
    let paths = LibraryPaths::new(root);
    let mut rows: Vec<(i64, String)> = Vec::new();
    database.with_reader(|connection| {
        for chunk in operation_ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let mut statement = connection.prepare(&format!(
                "SELECT id, path FROM folders WHERE purge_operation_id IN ({marks})"
            ))?;
            rows.extend(
                statement
                    .query_map(params_from_iter(chunk.iter()), |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(())
    })?;
    rows.sort_by_key(|(_, path)| std::cmp::Reverse(path.matches('/').count()));
    for (folder_id, path) in rows {
        let (images, children): (i64, i64) = database.with_reader(|connection| {
            let images: i64 = connection.query_row(
                "SELECT count(*) FROM images WHERE folder_id = ?1",
                [folder_id],
                |row| row.get(0),
            )?;
            let children: i64 = connection.query_row(
                "SELECT count(*) FROM folders WHERE path LIKE ?1 ESCAPE '\\'",
                [subtree_like_pattern(&path)],
                |row| row.get(0),
            )?;
            Ok((images, children))
        })?;
        if images > 0 || children > 0 {
            continue;
        }
        let directory = paths.folder_dir(&path);
        let deferred = match folder_dir_state(&paths, &path) {
            FolderDirState::Missing => None,
            FolderDirState::Empty => match std::fs::remove_dir(&directory) {
                Ok(()) => None,
                Err(error) => {
                    log::warn!(
                        "删除回收站空目录失败：{error}；目录位于 {}",
                        directory.display()
                    );
                    Some(format!("删除空目录失败：{error}"))
                }
            },
            FolderDirState::NotEmpty => {
                log::warn!(
                    "回收站文件夹目录里还有未纳管文件，保留待释放标记：{}",
                    directory.display()
                );
                Some("目录里还有未纳管文件，暂不删除；清空该目录后再清空回收站会重试".to_owned())
            }
            FolderDirState::Unreadable => {
                log::warn!(
                    "读取回收站目录失败，保留待释放标记：{}",
                    directory.display()
                );
                Some("无法读取目录，暂不删除".to_owned())
            }
            FolderDirState::OutsideRoot => {
                log::warn!(
                    "跳过图库外的回收站目录，保留待释放标记：{}",
                    directory.display()
                );
                Some("目录位于图库之外，暂不删除".to_owned())
            }
        };
        if let Some(reason) = deferred {
            report.failures.push(BatchFailure {
                image_id: None,
                path: path.clone(),
                reason,
            });
            continue;
        }
        database.with_durable_writer(|connection| {
            connection.execute(
                "DELETE FROM folders WHERE id = ?1 AND purge_operation_id IS NOT NULL",
                [folder_id],
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

/// 回收站文件夹目录的磁盘状态；只有「不存在」与「空」能在这一轮释放里删掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FolderDirState {
    Missing,
    Empty,
    NotEmpty,
    Unreadable,
    OutsideRoot,
}

/// 判断回收站文件夹目录能否直接删除。释放与「还有没有待释放文件」共用这一份口径，
/// 否则两边对「卡住的文件夹」判断不一致，会每次开库都白排一次释放任务。
pub(crate) fn folder_dir_state(paths: &LibraryPaths, folder_path: &str) -> FolderDirState {
    let directory = paths.folder_dir(folder_path);
    if !directory.exists() {
        return FolderDirState::Missing;
    }
    if ensure_dir_inside_root(&paths.root, &directory).is_err() {
        return FolderDirState::OutsideRoot;
    }
    match std::fs::read_dir(&directory) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                FolderDirState::NotEmpty
            } else {
                FolderDirState::Empty
            }
        }
        Err(_) => FolderDirState::Unreadable,
    }
}

pub(crate) fn content_path(
    paths: &LibraryPaths,
    folder_path: &str,
    content_hash: &str,
    file_ext: &str,
) -> AppResult<PathBuf> {
    if content_hash.len() != 64 || !content_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::message("内容哈希无效，拒绝操作文件"));
    }
    let normalized_folder = normalize_stored_folder(folder_path)?;
    if normalized_folder != folder_path {
        return Err(AppError::message("文件夹路径未规范化，拒绝操作文件"));
    }
    let extension = normalize_content_extension(file_ext)?;
    if extension != file_ext {
        return Err(AppError::message("扩展名未规范化，拒绝操作文件"));
    }
    Ok(paths.content_file(&normalized_folder, &format!("{content_hash}.{extension}")))
}

/// 目标目录必须已经存在，且 canonicalize 后仍是图库根目录或其子目录。
pub(super) fn ensure_dir_inside_root(root: &Path, directory: &Path) -> AppResult<()> {
    ensure_within(root, directory, "目标路径位于图库目录之外")
}

pub(crate) fn remove_content_file(root: &Path, path: &Path) -> AppResult<()> {
    let mut remover = ContentRemover::new(root)?;
    remover.validate(path)?;
    std::fs::remove_file(path)?;
    Ok(())
}

/// 批量删除原图时的路径守卫：根目录只 canonicalize 一次，父目录按文件夹缓存。
///
/// 批量压缩一次删几千张，每张都 canonicalize 根目录和父目录会明显拖慢收尾；
/// 这里只校验路径确实落在图库内，不改变删除语义。
pub(crate) struct ContentRemover {
    root: PathBuf,
    parents: HashSet<PathBuf>,
}

impl ContentRemover {
    pub(crate) fn new(root: &Path) -> AppResult<Self> {
        Ok(Self {
            root: root.canonicalize()?,
            parents: HashSet::new(),
        })
    }

    /// 校验目标路径的父目录在图库根目录内；同一目录只做一次 canonicalize。
    pub(crate) fn validate(&mut self, path: &Path) -> AppResult<()> {
        let parent = path
            .parent()
            .ok_or_else(|| AppError::message("目标路径没有父目录"))?;
        if self.parents.contains(parent) {
            return Ok(());
        }
        ensure_canonical_within(&self.root, parent, "目标路径位于图库目录之外")?;
        self.parents.insert(parent.to_path_buf());
        Ok(())
    }
}

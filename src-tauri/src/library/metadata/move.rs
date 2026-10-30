use std::path::Path;

use rusqlite::{Transaction, params, params_from_iter, types::Value};
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    library::{
        BatchReporter,
        db::LibraryDb,
        history::{self, HistoryKind, NewOperation},
        import::ensure_folder_path,
        paths::{LibraryPaths, normalize_folder},
        placeholders,
        service::now_ms,
        temp,
    },
};

use super::{
    MAX_BATCH_IDS, SQL_CHUNK,
    purge::{content_path, ensure_dir_inside_root, mark_for_purge},
    validate_ids,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchFailure {
    pub image_id: Option<i64>,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MoveReport {
    pub moved: u64,
    pub skipped: u64,
    pub failures: Vec<BatchFailure>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReport {
    pub deleted: u64,
    pub files_removed: u64,
    /// 永久删除/清空回收站只做标记时仍为 true；文件在记录过期或清空记录后释放。
    pub deferred: bool,
    pub failures: Vec<BatchFailure>,
}

pub fn move_images(
    database: &LibraryDb,
    root: &Path,
    image_ids: &[i64],
    folder_path: &str,
    reporter: &dyn BatchReporter,
) -> AppResult<MoveReport> {
    move_images_with_id(database, root, image_ids, folder_path, reporter).map(|(report, _)| report)
}

pub fn move_images_with_id(
    database: &LibraryDb,
    root: &Path,
    image_ids: &[i64],
    folder_path: &str,
    reporter: &dyn BatchReporter,
) -> AppResult<(MoveReport, Option<i64>)> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    // 临时路径在 normalize_folder 里就会被拒（保留目录名），不需要再查一次。
    let folder_path = normalize_folder(folder_path)?;
    database.with_reader(|connection| {
        temp::reject_images_in(
            connection,
            &ids,
            "临时图片不能移出临时文件夹；关闭或切换图库时会自动清空",
        )
    })?;
    let paths = LibraryPaths::new(root);
    let total = ids.len();
    reporter.report(0, total);

    history::write_with_id(
        database,
        NewOperation::new(HistoryKind::Move, format!("移动图片到「{folder_path}」")),
        |transaction, draft| {
            let mut report = MoveReport::default();
            let mut created_folders = Vec::new();
            // 目标文件夹可能已存在但大小写与请求不同：文件与行都必须落到库里的规范路径上。
            let (target_folder, target_path) =
                ensure_folder_path(transaction, &folder_path, now_ms(), &mut created_folders)?;
            let target_dir = if target_path.is_empty() {
                paths.root.clone()
            } else {
                paths
                    .root
                    .join(target_path.replace('/', std::path::MAIN_SEPARATOR_STR))
            };
            draft.journal().create_dir_all(&target_dir)?;
            ensure_dir_inside_root(&paths.root, &target_dir)?;

            let now = now_ms();
            let mut done = 0_usize;
            let mut moves = Vec::new();
            for chunk in ids.chunks(SQL_CHUNK) {
                if reporter.cancelled() {
                    break;
                }
                let marks = placeholders(chunk.len());
                let mut statement = transaction.prepare(&format!(
                    "SELECT images.id, images.content_hash, images.file_ext, folders.path,
                            images.folder_id, images.modified_at
                     FROM images JOIN folders ON folders.id = images.folder_id
                     WHERE images.id IN ({marks})"
                ))?;
                let rows = statement
                    .query_map(params_from_iter(chunk.iter()), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);

                for (
                    image_id,
                    content_hash,
                    file_ext,
                    old_folder,
                    old_folder_id,
                    old_modified_at,
                ) in rows
                {
                    if reporter.cancelled() {
                        break;
                    }
                    done += 1;
                    reporter.report(done, total);
                    if old_folder == target_path {
                        report.skipped += 1;
                        continue;
                    }
                    let old_path = content_path(&paths, &old_folder, &content_hash, &file_ext)?;
                    let new_path = content_path(&paths, &target_path, &content_hash, &file_ext)?;
                    if let Err(error) = std::fs::rename(&old_path, &new_path) {
                        report.failures.push(BatchFailure {
                            image_id: Some(image_id),
                            path: old_path.to_string_lossy().into_owned(),
                            reason: format!("移动文件失败：{error}"),
                        });
                        continue;
                    }
                    match transaction.execute(
                        "UPDATE images SET folder_id = ?1, modified_at = ?2 WHERE id = ?3",
                        params![target_folder, now, image_id],
                    ) {
                        Ok(changed) if changed > 0 => {
                            // 单张成功即记账；整批失败时由补偿日志逆序搬回。
                            draft.journal().renamed(&old_path, &new_path);
                            report.moved += 1;
                            moves.push((image_id, old_folder_id, target_folder, old_modified_at));
                        }
                        Ok(_) => {
                            let _ = std::fs::rename(&new_path, &old_path);
                            report.failures.push(BatchFailure {
                                image_id: Some(image_id),
                                path: new_path.to_string_lossy().into_owned(),
                                reason: "图片记录已不存在".to_owned(),
                            });
                        }
                        Err(error) => {
                            if let Err(rollback) = std::fs::rename(&new_path, &old_path) {
                                log::error!(
                                    "回滚文件夹移动失败 image={image_id}: {rollback}；文件位于 {}",
                                    new_path.display()
                                );
                            }
                            report.failures.push(BatchFailure {
                                image_id: Some(image_id),
                                path: new_path.to_string_lossy().into_owned(),
                                reason: format!("更新数据库失败：{error}"),
                            });
                        }
                    }
                }
            }
            draft.moves(&moves);
            if !created_folders.is_empty() {
                draft.json(
                    &created_folders
                        .into_iter()
                        .map(|(id, path)| history::FolderRef { id, path })
                        .collect::<Vec<_>>(),
                )?;
            }
            draft.affected(report.moved);
            Ok(report)
        },
    )
}

pub fn set_trashed(database: &LibraryDb, image_ids: &[i64], trashed: bool) -> AppResult<u64> {
    set_trashed_with_id(database, image_ids, trashed).map(|(changed, _)| changed)
}

/// 恢复单张图片的守卫：图片所在文件夹整目录在回收站里时拒绝，避免出现
/// 「正常图片位于已删除文件夹」——那种行会让空回收站永远释放不掉这个文件夹。
fn reject_restore_from_trashed_folder(transaction: &Transaction<'_>, ids: &[i64]) -> AppResult<()> {
    for chunk in ids.chunks(SQL_CHUNK) {
        let marks = placeholders(chunk.len());
        let count: i64 = transaction.query_row(
            &format!(
                "SELECT count(*) FROM images
                 JOIN folders ON folders.id = images.folder_id
                 WHERE images.id IN ({marks}) AND folders.trashed_at IS NOT NULL"
            ),
            params_from_iter(chunk.iter()),
            |row| row.get(0),
        )?;
        if count > 0 {
            return Err(AppError::message(
                "已删除文件夹下的图片不能单张恢复；请恢复整个文件夹",
            ));
        }
    }
    Ok(())
}

pub fn set_trashed_with_id(
    database: &LibraryDb,
    image_ids: &[i64],
    trashed: bool,
) -> AppResult<(u64, Option<i64>)> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    temp::reject_images(
        database,
        &ids,
        "临时图片不能移入回收站或恢复；关闭或切换图库时会自动清空",
    )?;
    let (kind, label) = if trashed {
        (HistoryKind::Trash, "移入回收站")
    } else {
        (HistoryKind::Restore, "恢复图片")
    };
    // 只处理仍处于目标状态的行；待释放行不参与，否则会把标记中的图片恢复成正常图片。
    let matches = if trashed {
        "deleted_at IS NULL"
    } else {
        "deleted_at IS NOT NULL"
    };
    let assigned = if trashed { "?1" } else { "NULL" };
    history::write_with_id(
        database,
        NewOperation::new(kind, label),
        |transaction, draft| {
            if !trashed {
                // 必须在写事务里判断：文件夹整目录删除也走同一条写连接，
                // 事务外检查会与它擦肩而过，留下「正常图片位于已删除文件夹」的非法状态。
                reject_restore_from_trashed_folder(transaction, &ids)?;
            }
            let now = now_ms();
            let mut triples = Vec::new();
            let mut changed = 0_u64;
            for chunk in ids.chunks(SQL_CHUNK) {
                let marks = placeholders(chunk.len());
                let mut statement = transaction.prepare(&format!(
                    "SELECT id, deleted_at, modified_at FROM images
                     WHERE id IN ({marks}) AND {matches} AND purge_operation_id IS NULL"
                ))?;
                let rows = statement
                    .query_map(params_from_iter(chunk.iter()), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);
                for (image_id, old_deleted_at, old_modified_at) in rows {
                    triples.push((image_id, old_deleted_at.unwrap_or(0), old_modified_at));
                }
                changed += transaction.execute(
                    &format!(
                        "UPDATE images SET deleted_at = {assigned}, modified_at = ?1
                         WHERE id IN ({marks}) AND {matches}
                           AND purge_operation_id IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(now))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )? as u64;
            }
            draft.triples(&triples);
            draft.affected(changed);
            Ok(changed)
        },
    )
}

/// 永久删除：整批都必须在回收站里，否则拒绝（避免绕过界面隐藏正常图片）。
/// 只写待释放标记，原图与数据库行由释放任务删除。
pub fn delete_images(
    database: &LibraryDb,
    image_ids: &[i64],
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    mark_for_purge(database, Some(&ids), reporter)
}

/// 清空回收站：给全部可删图片与回收站文件夹写待释放标记，文件同样等释放任务。
pub fn empty_trash(database: &LibraryDb, reporter: &dyn BatchReporter) -> AppResult<DeleteReport> {
    mark_for_purge(database, None, reporter)
}

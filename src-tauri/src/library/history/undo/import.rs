use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::TaskHandle,
    },
    library::{db::LibraryDb, folders, fs_journal::FileJournal, metadata, paths::LibraryPaths},
};

use super::super::{
    ChunkFormat, ImportMerged, ImportRecord, ImportRedoImage, ImportRedoMerged, ImportRedoTag,
    ImportTag, ImportedRow, RedoRecord,
    chunks::load_chunks,
    replay::{append_redo, load_redo_records},
    undo_dir,
};
use super::folders::folder_rows;

/// 导入撤销：暂存原图并记录完整行，删除新增行，恢复合并项快照，再清理本次新建且仍为空/无引用的文件夹与标签。
pub(crate) fn undo_import(
    database: &LibraryDb,
    root: &Path,
    task: &TaskHandle,
    operation_id: i64,
) -> AppResult<()> {
    let chunks = database.with_reader(|connection| load_chunks(connection, operation_id))?;
    let mut created_folders = Vec::new();
    let mut created_tags: Vec<ImportTag> = Vec::new();
    let mut merged: Vec<ImportMerged> = Vec::new();
    for chunk in chunks {
        if chunk.format != ChunkFormat::Json {
            continue;
        }
        match serde_json::from_slice::<ImportRecord>(&chunk.data)? {
            ImportRecord::Folders { items } => created_folders.extend(items),
            ImportRecord::Tags { items } => created_tags.extend(items),
            ImportRecord::Merged { items } => merged.extend(items),
        }
    }

    let mut deleted = 0_usize;
    // 续做时也要知道上一轮已经删掉的原导入行；redo chunks 就是这份清单。
    let mut removed_ids: HashSet<i64> = database.with_reader(|connection| {
        let mut ids = HashSet::new();
        for record in load_redo_records(connection, operation_id)? {
            if let RedoRecord::ImportImages { items } = record {
                ids.extend(items.into_iter().map(|item| item.row.id));
            }
        }
        Ok(ids)
    })?;
    loop {
        let rows = database.with_reader(|connection| load_import_rows(connection, operation_id))?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows.iter().map(|(row, _)| row.id).collect();
        removed_ids.extend(ids.iter().copied());
        let tag_ids = import_tag_relations(database, &ids)?;
        let collection_items = import_collection_relations(database, &ids)?;

        let mut items = Vec::with_capacity(rows.len());
        let mut staged_moves: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (row, folder_path) in rows {
            let tags = tag_ids.get(&row.id).cloned().unwrap_or_default();
            let collections = collection_items.get(&row.id).cloned().unwrap_or_default();
            let (staged_file, moved) = stage_import_file(root, operation_id, &row, &folder_path)?;
            if let Some((source, staged)) = moved {
                staged_moves.push((source, staged));
            }
            items.push(ImportRedoImage {
                row,
                folder_path,
                tag_ids: tags,
                collection_items: collections,
                staged_file,
            });
        }

        let result = database.with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::ImportImages { items },
            )?;
            let marks = crate::library::placeholders(ids.len());
            transaction.execute(
                &format!("DELETE FROM images WHERE id IN ({marks})"),
                rusqlite::params_from_iter(ids.iter()),
            )?;
            transaction.commit()?;
            Ok(())
        });
        if let Err(error) = result {
            // 行还在，把已经搬进暂存的文件放回原位置，保持可重试。
            for (source, staged) in staged_moves.iter().rev() {
                if staged.is_file() && !source.exists() {
                    let _ = fs::rename(staged, source);
                }
            }
            return Err(error);
        }
        deleted += ids.len();
        task.set_message(format!("撤销导入：已移除 {deleted} 张"));
    }

    if !merged.is_empty() {
        restore_import_merges(database, operation_id, &merged, &removed_ids)?;
    }

    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let folder_rows = folder_rows(&transaction, &created_folders)?;
        if !folder_rows.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::FolderRows { items: folder_rows },
            )?;
        }
        if !created_tags.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::ImportTags {
                    items: created_tags
                        .iter()
                        .map(|tag| ImportRedoTag {
                            id: tag.id,
                            name: tag.name.clone(),
                        })
                        .collect(),
                },
            )?;
        }
        let mut journal = FileJournal::default();
        let result = (|| -> AppResult<()> {
            folders::cleanup_created(&transaction, root, &created_folders, &mut journal)?;
            for tag in &created_tags {
                let name: Option<String> = transaction
                    .query_row("SELECT name FROM tags WHERE id = ?1", [tag.id], |row| {
                        row.get(0)
                    })
                    .optional()?;
                // 标签 id 也可能被复用；名字对不上就保留，避免删掉别的标签。
                if !name.is_some_and(|name| name.eq_ignore_ascii_case(&tag.name)) {
                    continue;
                }
                let used: i64 = transaction.query_row(
                    "SELECT count(*) FROM image_tags WHERE tag_id = ?1",
                    [tag.id],
                    |row| row.get(0),
                )?;
                if used == 0 {
                    transaction.execute("DELETE FROM tags WHERE id = ?1", [tag.id])?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            journal.rollback();
            return Err(error);
        }
        if let Err(error) = transaction.commit() {
            journal.rollback();
            return Err(error.into());
        }
        Ok(())
    })
}

pub(crate) fn redo_import(
    database: &LibraryDb,
    root: &Path,
    task: &TaskHandle,
    operation_id: i64,
) -> AppResult<()> {
    let records = database.with_reader(|connection| load_redo_records(connection, operation_id))?;
    let mut images = Vec::new();
    let mut tags = Vec::new();
    let mut folders = Vec::new();
    let mut merged = Vec::new();
    for record in records {
        match record {
            RedoRecord::ImportImages { items } => images.extend(items),
            RedoRecord::ImportTags { items } => tags.extend(items),
            RedoRecord::FolderRows { items } => folders.extend(items),
            RedoRecord::ImportMerged { items } => merged.extend(items),
            _ => {}
        }
    }
    if images.is_empty() && merged.is_empty() && folders.is_empty() && tags.is_empty() {
        return Err(AppError::message("这条记录缺少重做数据"));
    }

    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut journal = FileJournal::default();
        let result = (|| -> AppResult<()> {
            folders::redo_create(&transaction, root, &folders, &mut journal)?;
            ensure_import_tags(&transaction, &tags)?;
            Ok(())
        })();
        if let Err(error) = result {
            journal.rollback();
            return Err(error);
        }
        if let Err(error) = transaction.commit() {
            journal.rollback();
            return Err(error.into());
        }
        Ok(())
    })?;

    let total = images.len();
    let mut restored = 0_usize;
    let mut missing = 0_usize;
    for batch in images.chunks(200) {
        for item in batch {
            // 缺原图只影响这一张：行照常恢复，文件留空，不能中断整条重做链。
            if let ImportFileRestore::Missing(path) = restore_import_file(root, operation_id, item)?
            {
                log::warn!(
                    "重做导入：图片 {} 的原图缺失，已按缺图恢复（预期位置 {}）",
                    item.row.id,
                    path.display()
                );
                task.add_failure(
                    Some(item.row.id),
                    path.to_string_lossy().into_owned(),
                    "原图文件缺失，已按缺图恢复".to_owned(),
                );
                missing += 1;
            }
        }
        database.with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for item in batch {
                let existing: Option<(i64, String)> = transaction
                    .query_row(
                        "SELECT id, content_hash FROM images WHERE id = ?1 OR content_hash = ?2",
                        params![item.row.id, item.row.content_hash],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((id, hash)) = existing {
                    if id != item.row.id || hash != item.row.content_hash {
                        return Err(AppError::message(format!(
                            "重做导入时图片 {} 的内容已被占用",
                            item.row.id
                        )));
                    }
                } else {
                    insert_import_row(&transaction, &item.row)?;
                }
                for tag_id in &item.tag_ids {
                    transaction.execute(
                        "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)
                         ON CONFLICT(image_id, tag_id) DO NOTHING",
                        params![item.row.id, tag_id],
                    )?;
                }
                for (collection_id, position) in &item.collection_items {
                    transaction.execute(
                        "INSERT INTO collection_items(collection_id, image_id, position)
                         SELECT ?1, ?2, ?3 WHERE EXISTS (
                             SELECT 1 FROM collections WHERE id = ?1
                         )
                         ON CONFLICT(collection_id, image_id) DO NOTHING",
                        params![collection_id, item.row.id, position],
                    )?;
                }
            }
            transaction.commit()?;
            Ok(())
        })?;
        restored += batch.len();
        if missing > 0 {
            task.set_message(format!(
                "重做导入：已恢复 {restored}/{total} 张，{missing} 张缺原图"
            ));
        } else {
            task.set_message(format!("重做导入：已恢复 {restored}/{total} 张"));
        }
    }

    if !merged.is_empty() {
        redo_import_merges(database, &merged)?;
    }
    Ok(())
}

fn load_import_rows(
    connection: &Connection,
    operation_id: i64,
) -> AppResult<Vec<(ImportedRow, String)>> {
    let mut statement = connection.prepare(
        "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                images.file_size, images.width, images.height, images.format,
                images.has_alpha, images.phash, images.colors4, images.colors8,
                images.score, images.description, images.description_source,
                images.description_model, images.imported_at, images.modified_at,
                images.deleted_at, images.thumb_segment, images.thumb_offset,
                images.thumb_length, images.thumb_format, images.thumb_width,
                images.thumb_height, images.thumb_version, images.origin_operation_id,
                folders.path
         FROM images JOIN folders ON folders.id = images.folder_id
         WHERE images.origin_operation_id = ?1
         ORDER BY images.id
         LIMIT 400",
    )?;
    let rows = statement
        .query_map([operation_id], |row| {
            Ok((
                ImportedRow {
                    id: row.get(0)?,
                    content_hash: row.get(1)?,
                    file_ext: row.get(2)?,
                    folder_id: row.get(3)?,
                    file_size: row.get(4)?,
                    width: row.get(5)?,
                    height: row.get(6)?,
                    format: row.get(7)?,
                    has_alpha: row.get::<_, i64>(8)? != 0,
                    phash: row.get(9)?,
                    colors4: row.get(10)?,
                    colors8: row.get(11)?,
                    score: row.get(12)?,
                    description: row.get(13)?,
                    description_source: row.get(14)?,
                    description_model: row.get(15)?,
                    imported_at: row.get(16)?,
                    modified_at: row.get(17)?,
                    deleted_at: row.get(18)?,
                    thumb_segment: row.get(19)?,
                    thumb_offset: row.get(20)?,
                    thumb_length: row.get(21)?,
                    thumb_format: row.get(22)?,
                    thumb_width: row.get(23)?,
                    thumb_height: row.get(24)?,
                    thumb_version: row.get(25)?,
                    origin_operation_id: row.get(26)?,
                },
                row.get(27)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn import_tag_relations(database: &LibraryDb, ids: &[i64]) -> AppResult<HashMap<i64, Vec<i64>>> {
    let mut relations: HashMap<i64, Vec<i64>> = HashMap::new();
    if ids.is_empty() {
        return Ok(relations);
    }
    database.with_reader(|connection| {
        let marks = crate::library::placeholders(ids.len());
        let mut statement = connection.prepare(&format!(
            "SELECT image_id, tag_id FROM image_tags
             WHERE image_id IN ({marks}) ORDER BY image_id, tag_id"
        ))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (image_id, tag_id) in rows {
            relations.entry(image_id).or_default().push(tag_id);
        }
        Ok(())
    })?;
    Ok(relations)
}

fn import_collection_relations(
    database: &LibraryDb,
    ids: &[i64],
) -> AppResult<HashMap<i64, Vec<(i64, i64)>>> {
    let mut relations: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    if ids.is_empty() {
        return Ok(relations);
    }
    database.with_reader(|connection| {
        let marks = crate::library::placeholders(ids.len());
        let mut statement = connection.prepare(&format!(
            "SELECT image_id, collection_id, position FROM collection_items
             WHERE image_id IN ({marks}) ORDER BY image_id, collection_id"
        ))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (image_id, collection_id, position) in rows {
            relations
                .entry(image_id)
                .or_default()
                .push((collection_id, position));
        }
        Ok(())
    })?;
    Ok(relations)
}

/// 导入原图暂存结果：撤销目录里的文件名，以及本次真的搬动过的 `(原路径, 暂存路径)`；
/// 写事务失败时按后者把文件放回原处。
type StagedImportFile = (Option<String>, Option<(PathBuf, PathBuf)>);

fn stage_import_file(
    root: &Path,
    operation_id: i64,
    row: &ImportedRow,
    folder_path: &str,
) -> AppResult<StagedImportFile> {
    let name = format!("import-{}.{}", row.id, row.file_ext);
    let staged = undo_dir(root, operation_id).join(&name);
    if staged.is_file() {
        return Ok((Some(name), None));
    }
    let source = metadata::content_path(
        &LibraryPaths::new(root),
        folder_path,
        &row.content_hash,
        &row.file_ext,
    )?;
    if !source.is_file() {
        return Ok((None, None));
    }
    if let Some(parent) = staged.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(&source, &staged) {
        Ok(()) => Ok((Some(name), Some((source, staged)))),
        Err(_) => {
            fs::copy(&source, &staged)?;
            fs::remove_file(&source)?;
            Ok((Some(name), Some((source, staged))))
        }
    }
}

/// 重做导入时原图文件的恢复结果。
enum ImportFileRestore {
    Restored,
    /// 撤销时原图就已经不在了（或暂存文件被清理）：行照常恢复，文件留空，
    /// 由检查图库按「缺失」报出来，不阻塞同一条记录里其它图片的重做。
    Missing(PathBuf),
}

fn restore_import_file(
    root: &Path,
    operation_id: i64,
    item: &ImportRedoImage,
) -> AppResult<ImportFileRestore> {
    let destination = metadata::content_path(
        &LibraryPaths::new(root),
        &item.folder_path,
        &item.row.content_hash,
        &item.row.file_ext,
    )?;
    if destination.is_file() {
        return Ok(ImportFileRestore::Restored);
    }
    let Some(name) = &item.staged_file else {
        // 撤销时原图就已经不在了：由调用方统一记一条失败清单。
        return Ok(ImportFileRestore::Missing(destination));
    };
    let staged = undo_dir(root, operation_id).join(name);
    if !staged.is_file() {
        log::warn!(
            "重做导入时图片 {} 的暂存文件已不存在，按缺图恢复",
            item.row.id
        );
        return Ok(ImportFileRestore::Missing(destination));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&staged, &destination)?;
    Ok(ImportFileRestore::Restored)
}

fn insert_import_row(transaction: &Transaction<'_>, row: &ImportedRow) -> AppResult<()> {
    transaction.execute(
        "INSERT INTO images(
            id, content_hash, file_ext, folder_id, file_size, width, height, format,
            has_alpha, phash, colors4, colors8, score, description, description_source,
            description_model, imported_at, modified_at, deleted_at, thumb_segment,
            thumb_offset, thumb_length, thumb_format, thumb_width, thumb_height,
            thumb_version, origin_operation_id
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27
         )",
        params![
            row.id,
            row.content_hash,
            row.file_ext,
            row.folder_id,
            row.file_size,
            row.width,
            row.height,
            row.format,
            row.has_alpha,
            row.phash,
            row.colors4,
            row.colors8,
            row.score,
            row.description,
            row.description_source,
            row.description_model,
            row.imported_at,
            row.modified_at,
            row.deleted_at,
            row.thumb_segment,
            row.thumb_offset,
            row.thumb_length,
            row.thumb_format,
            row.thumb_width,
            row.thumb_height,
            row.thumb_version,
            row.origin_operation_id,
        ],
    )?;
    Ok(())
}

fn ensure_import_tags(transaction: &Transaction<'_>, tags: &[ImportRedoTag]) -> AppResult<()> {
    for tag in tags {
        let existing: Option<String> = transaction
            .query_row("SELECT name FROM tags WHERE id = ?1", [tag.id], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(name) = existing {
            if !name.eq_ignore_ascii_case(&tag.name) {
                return Err(AppError::message(format!(
                    "重做导入时标签 id {} 已被其他标签占用",
                    tag.id
                )));
            }
            continue;
        }
        let occupied: Option<i64> = transaction
            .query_row(
                "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
                [&tag.name],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(other) = occupied {
            return Err(AppError::message(format!(
                "重做导入时标签「{}」已被 id {other} 占用",
                tag.name
            )));
        }
        transaction.execute(
            "INSERT INTO tags(id, name) VALUES (?1, ?2)",
            params![tag.id, tag.name],
        )?;
    }
    Ok(())
}

fn restore_import_merges(
    database: &LibraryDb,
    operation_id: i64,
    merged: &[ImportMerged],
    removed_ids: &HashSet<i64>,
) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut redo = Vec::new();
        // 同一次导入里同一张图可能被合并多次（一次导入里有两份重复内容）。
        // 每条记录存的是「合并前」的快照，只有最早那份才是导入前的状态；
        // 按记录顺序全应用会把图片停在中间态，所以每张图只应用第一条。
        let mut restored: HashSet<i64> = HashSet::new();
        for item in merged {
            if !restored.insert(item.image_id) {
                continue;
            }
            let current: Option<(i64, i64)> = transaction
                .query_row(
                    "SELECT score, modified_at FROM images WHERE id = ?1",
                    [item.image_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((score, modified_at)) = current else {
                // 本次导入新增、又在同一批里被合并的行已经随导入一起删除。
                if removed_ids.contains(&item.image_id) {
                    continue;
                }
                return Err(AppError::message(format!(
                    "导入合并的图片 {} 已不存在",
                    item.image_id
                )));
            };
            let current_tags = import_tag_ids(&transaction, item.image_id)?;
            let mut old_tags = item.old_tag_ids.clone();
            old_tags.sort_unstable();
            if score == item.old_score
                && modified_at == item.old_modified_at
                && current_tags == old_tags
            {
                continue;
            }
            redo.push(ImportRedoMerged {
                image_id: item.image_id,
                score,
                modified_at,
                tag_ids: current_tags,
            });
            transaction.execute(
                "UPDATE images SET score = ?1, modified_at = ?2 WHERE id = ?3",
                params![item.old_score, item.old_modified_at, item.image_id],
            )?;
            transaction.execute(
                "DELETE FROM image_tags WHERE image_id = ?1",
                [item.image_id],
            )?;
            for tag_id in &item.old_tag_ids {
                transaction.execute(
                    "INSERT INTO image_tags(image_id, tag_id)
                     SELECT ?1, id FROM tags WHERE id = ?2
                     ON CONFLICT(image_id, tag_id) DO NOTHING",
                    params![item.image_id, tag_id],
                )?;
            }
        }
        if !redo.is_empty() {
            append_redo(
                &transaction,
                operation_id,
                &RedoRecord::ImportMerged { items: redo },
            )?;
        }
        transaction.commit()?;
        Ok(())
    })
}

fn import_tag_ids(connection: &Connection, image_id: i64) -> AppResult<Vec<i64>> {
    let mut statement =
        connection.prepare("SELECT tag_id FROM image_tags WHERE image_id = ?1 ORDER BY tag_id")?;
    let ids = statement
        .query_map([image_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

fn redo_import_merges(database: &LibraryDb, merged: &[ImportRedoMerged]) -> AppResult<()> {
    database.with_writer(|connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for item in merged {
            let changed = transaction.execute(
                "UPDATE images SET score = ?1, modified_at = ?2 WHERE id = ?3",
                params![item.score, item.modified_at, item.image_id],
            )?;
            if changed == 0 {
                return Err(AppError::message(format!(
                    "重做导入合并时图片 {} 已不存在",
                    item.image_id
                )));
            }
            transaction.execute(
                "DELETE FROM image_tags WHERE image_id = ?1",
                [item.image_id],
            )?;
            for tag_id in &item.tag_ids {
                transaction.execute(
                    "INSERT INTO image_tags(image_id, tag_id)
                     SELECT ?1, id FROM tags WHERE id = ?2
                     ON CONFLICT(image_id, tag_id) DO NOTHING",
                    params![item.image_id, tag_id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    })
}

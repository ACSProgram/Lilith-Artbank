use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    library::{
        BatchReporter,
        db::LibraryDb,
        import::{ensure_folder, ensure_tags},
        paths::{LibraryPaths, normalize_content_extension, normalize_folder},
        placeholders,
        service::now_ms,
    },
};

const SQL_CHUNK: usize = 400;
const MAX_BATCH_IDS: usize = 1_000_000;
const MAX_FILE_BATCH_IDS: usize = 10_000;
const MAX_TAGS_PER_IMAGE: usize = 64;
const MAX_TAG_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 50_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorEntry {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub weight: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDetails {
    pub id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub file_path: String,
    pub folder_id: i64,
    pub folder_path: String,
    pub file_size: i64,
    pub width: i64,
    pub height: i64,
    pub format: String,
    pub has_alpha: bool,
    pub score: i64,
    pub description: String,
    pub description_source: String,
    pub description_model: String,
    pub imported_at: i64,
    pub modified_at: i64,
    pub deleted_at: Option<i64>,
    pub thumb_version: i64,
    pub thumb_width: i64,
    pub thumb_height: i64,
    pub tags: Vec<TagRef>,
    pub colors4: Vec<ColorEntry>,
    pub colors8: Vec<ColorEntry>,
}

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
    pub failures: Vec<BatchFailure>,
}

pub fn image_details(database: &LibraryDb, root: &Path, image_id: i64) -> AppResult<ImageDetails> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    database.with_reader(|connection| {
        let mut details = connection
            .query_row(
                "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                        folders.path, images.file_size, images.width, images.height,
                        images.format, images.has_alpha, images.score, images.description,
                        images.description_source, images.description_model,
                        images.imported_at, images.modified_at, images.deleted_at,
                        images.thumb_version, images.thumb_width, images.thumb_height,
                        images.colors4, images.colors8
                 FROM images
                 JOIN folders ON folders.id = images.folder_id
                 WHERE images.id = ?1",
                [image_id],
                |row| {
                    Ok(ImageDetails {
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
                        score: row.get(10)?,
                        description: row.get(11)?,
                        description_source: row.get(12)?,
                        description_model: row.get(13)?,
                        imported_at: row.get(14)?,
                        modified_at: row.get(15)?,
                        deleted_at: row.get(16)?,
                        thumb_version: row.get(17)?,
                        thumb_width: row.get(18)?,
                        thumb_height: row.get(19)?,
                        tags: Vec::new(),
                        colors4: parse_palette(&row.get::<_, Vec<u8>>(20)?),
                        colors8: parse_palette(&row.get::<_, Vec<u8>>(21)?),
                        file_path: String::new(),
                    })
                },
            )
            .optional()?
            .ok_or_else(|| AppError::message("图片不存在"))?;

        let file_name = format!("{}.{}", details.content_hash, details.file_ext);
        details.file_path = LibraryPaths::new(root)
            .content_file(&details.folder_path, &file_name)
            .to_string_lossy()
            .into_owned();

        let mut statement = connection.prepare(
            "SELECT tags.id, tags.name
             FROM image_tags
             JOIN tags ON tags.id = image_tags.tag_id
             WHERE image_tags.image_id = ?1
             ORDER BY tags.name COLLATE NOCASE",
        )?;
        details.tags = statement
            .query_map([image_id], |row| {
                Ok(TagRef {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(details)
    })
}

fn parse_palette(bytes: &[u8]) -> Vec<ColorEntry> {
    bytes
        .chunks_exact(4)
        .map(|slot| ColorEntry {
            r: slot[0],
            g: slot[1],
            b: slot[2],
            weight: slot[3],
        })
        .collect()
}

pub fn update_score(database: &LibraryDb, image_id: i64, score: i64) -> AppResult<()> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    validate_score(score)?;
    database.with_writer(|connection| {
        let changed = connection.execute(
            "UPDATE images SET score = ?1, modified_at = ?2 WHERE id = ?3",
            params![score, now_ms(), image_id],
        )?;
        if changed == 0 {
            return Err(AppError::message("图片不存在"));
        }
        Ok(())
    })
}

pub fn set_scores(database: &LibraryDb, image_ids: &[i64], score: i64) -> AppResult<u64> {
    validate_score(score)?;
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    database.with_writer(|connection| {
        let now = now_ms();
        let mut changed = 0_u64;
        for chunk in ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            changed += connection.execute(
                &format!("UPDATE images SET score = ?1, modified_at = ?2 WHERE id IN ({marks})"),
                params_from_iter(
                    std::iter::once(Value::Integer(score))
                        .chain(std::iter::once(Value::Integer(now)))
                        .chain(chunk.iter().map(|id| Value::Integer(*id))),
                ),
            )? as u64;
        }
        Ok(changed)
    })
}

pub fn update_description(database: &LibraryDb, image_id: i64, description: &str) -> AppResult<()> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    let description = description.trim();
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(AppError::message(format!(
            "描述不能超过 {MAX_DESCRIPTION_CHARS} 个字符"
        )));
    }
    database.with_writer(|connection| {
        let changed = connection.execute(
            "UPDATE images
             SET description = ?1, description_source = 'user', description_model = '',
                 modified_at = ?2
             WHERE id = ?3",
            params![description, now_ms(), image_id],
        )?;
        if changed == 0 {
            return Err(AppError::message("图片不存在"));
        }
        Ok(())
    })
}

/// AI 描述只覆盖空描述或非用户来源；手写描述默认受保护。
pub fn set_ai_description(
    database: &LibraryDb,
    image_id: i64,
    description: &str,
    model: &str,
) -> AppResult<bool> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    let description = description.trim();
    if description.is_empty() {
        return Err(AppError::message("AI 描述为空"));
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(AppError::message(format!(
            "描述不能超过 {MAX_DESCRIPTION_CHARS} 个字符"
        )));
    }
    database.with_writer(|connection| {
        let changed = connection.execute(
            "UPDATE images
             SET description = ?1, description_source = 'ai', description_model = ?2,
                 modified_at = ?3
             WHERE id = ?4
               AND (description = '' OR description_source <> 'user')",
            params![description, model, now_ms(), image_id],
        )?;
        Ok(changed > 0)
    })
}

pub fn set_image_tags(
    database: &LibraryDb,
    image_id: i64,
    tag_names: &[String],
) -> AppResult<Vec<TagRef>> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    let names = normalize_tag_names(tag_names)?;
    database.with_writer(|connection| {
        let transaction = connection.transaction()?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM images WHERE id = ?1)",
            [image_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(AppError::message("图片不存在"));
        }
        let tag_ids = ensure_tags(&transaction, &names)?;
        transaction.execute("DELETE FROM image_tags WHERE image_id = ?1", [image_id])?;
        for tag_id in &tag_ids {
            transaction.execute(
                "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)",
                params![image_id, tag_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    })?;
    tag_refs(database, image_id)
}

pub fn add_tags(database: &LibraryDb, image_ids: &[i64], tag_names: &[String]) -> AppResult<u64> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    let names = normalize_tag_names(tag_names)?;
    if names.is_empty() {
        return Ok(0);
    }
    database.with_writer(|connection| {
        let transaction = connection.transaction()?;
        let tag_ids = ensure_tags(&transaction, &names)?;
        let mut added = 0_u64;
        for tag_id in &tag_ids {
            for chunk in ids.chunks(SQL_CHUNK) {
                let marks = placeholders(chunk.len());
                added += transaction.execute(
                    &format!(
                        "INSERT INTO image_tags(image_id, tag_id)
                         SELECT images.id, ?1 FROM images WHERE images.id IN ({marks})
                         ON CONFLICT(image_id, tag_id) DO NOTHING"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(*tag_id))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )? as u64;
            }
        }
        transaction.commit()?;
        Ok(added)
    })
}

pub fn remove_tags(
    database: &LibraryDb,
    image_ids: &[i64],
    tag_names: &[String],
) -> AppResult<u64> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    let names = normalize_tag_names(tag_names)?;
    if names.is_empty() {
        return Ok(0);
    }
    database.with_writer(|connection| {
        let transaction = connection.transaction()?;
        let tag_ids = tag_ids_for_names(&transaction, &names)?;
        if tag_ids.is_empty() {
            return Ok(0);
        }
        let tag_marks = placeholders(tag_ids.len());
        let mut removed = 0_u64;
        for chunk in ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            removed += transaction.execute(
                &format!(
                    "DELETE FROM image_tags
                     WHERE tag_id IN ({tag_marks}) AND image_id IN ({marks})"
                ),
                params_from_iter(
                    tag_ids
                        .iter()
                        .map(|id| Value::Integer(*id))
                        .chain(chunk.iter().map(|id| Value::Integer(*id))),
                ),
            )? as u64;
        }
        transaction.commit()?;
        Ok(removed)
    })
}

pub fn rename_tag(database: &LibraryDb, tag_id: i64, name: &str) -> AppResult<()> {
    if tag_id <= 0 {
        return Err(AppError::message("标签 id 无效"));
    }
    let name = validate_tag_name(name)?;
    database.with_writer(|connection| {
        let duplicate: Option<i64> = connection
            .query_row(
                "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE AND id <> ?2",
                params![name, tag_id],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            return Err(AppError::message("已经有同名标签"));
        }
        let changed = connection.execute(
            "UPDATE tags SET name = ?1 WHERE id = ?2",
            params![name, tag_id],
        )?;
        if changed == 0 {
            return Err(AppError::message("标签不存在"));
        }
        Ok(())
    })
}

pub fn delete_tag(database: &LibraryDb, tag_id: i64) -> AppResult<u64> {
    if tag_id <= 0 {
        return Err(AppError::message("标签 id 无效"));
    }
    database.with_writer(|connection| {
        let affected: i64 = connection.query_row(
            "SELECT count(*) FROM image_tags WHERE tag_id = ?1",
            [tag_id],
            |row| row.get(0),
        )?;
        let changed = connection.execute("DELETE FROM tags WHERE id = ?1", [tag_id])?;
        if changed == 0 {
            return Err(AppError::message("标签不存在"));
        }
        Ok(affected as u64)
    })
}

pub fn move_images(
    database: &LibraryDb,
    root: &Path,
    image_ids: &[i64],
    folder_path: &str,
    reporter: &dyn BatchReporter,
) -> AppResult<MoveReport> {
    let ids = validate_ids_for_files(image_ids)?;
    let folder_path = normalize_folder(folder_path)?;
    let paths = LibraryPaths::new(root);
    let mut report = MoveReport::default();
    let total = ids.len();
    reporter.report(0, total);

    database.with_writer(|connection| {
        let transaction = connection.transaction()?;
        let target_folder = ensure_folder(&transaction, &folder_path, now_ms())?;
        let target_dir = if folder_path.is_empty() {
            paths.root.clone()
        } else {
            paths
                .root
                .join(folder_path.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        std::fs::create_dir_all(&target_dir)?;
        ensure_dir_inside_root(&paths.root, &target_dir)?;

        let now = now_ms();
        let mut done = 0_usize;
        for chunk in ids.chunks(SQL_CHUNK) {
            if reporter.cancelled() {
                break;
            }
            let marks = placeholders(chunk.len());
            let mut statement = transaction.prepare(&format!(
                "SELECT images.id, images.content_hash, images.file_ext, folders.path
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
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);

            for (image_id, content_hash, file_ext, old_folder) in rows {
                if reporter.cancelled() {
                    break;
                }
                done += 1;
                reporter.report(done, total);
                if old_folder == folder_path {
                    report.skipped += 1;
                    continue;
                }
                let old_path = content_path(&paths, &old_folder, &content_hash, &file_ext)?;
                let new_path = content_path(&paths, &folder_path, &content_hash, &file_ext)?;
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
                    Ok(changed) if changed > 0 => report.moved += 1,
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
        transaction.commit()?;
        Ok(())
    })?;
    Ok(report)
}

pub fn set_trashed(database: &LibraryDb, image_ids: &[i64], trashed: bool) -> AppResult<u64> {
    let ids = validate_ids(image_ids, MAX_BATCH_IDS)?;
    database.with_writer(|connection| {
        let now = now_ms();
        let mut changed = 0_u64;
        for chunk in ids.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            changed += if trashed {
                connection.execute(
                    &format!(
                        "UPDATE images SET deleted_at = ?1, modified_at = ?1
                         WHERE id IN ({marks}) AND deleted_at IS NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(now))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )?
            } else {
                connection.execute(
                    &format!(
                        "UPDATE images SET deleted_at = NULL, modified_at = ?1
                         WHERE id IN ({marks}) AND deleted_at IS NOT NULL"
                    ),
                    params_from_iter(
                        std::iter::once(Value::Integer(now))
                            .chain(chunk.iter().map(|id| Value::Integer(*id))),
                    ),
                )?
            } as u64;
        }
        Ok(changed)
    })
}

pub fn delete_images(
    database: &LibraryDb,
    root: &Path,
    image_ids: &[i64],
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    let ids = validate_ids_for_files(image_ids)?;
    delete_internal(database, root, Some(&ids), false, reporter)
}

pub fn empty_trash(
    database: &LibraryDb,
    root: &Path,
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    delete_internal(database, root, None, true, reporter)
}

/// 按块删除：每块先删数据库行并提交，再删除对应原图。
/// 取消发生在块边界，已完成的块保持“行与文件都已删除”，不会留下半删状态。
fn delete_internal(
    database: &LibraryDb,
    root: &Path,
    image_ids: Option<&[i64]>,
    only_trashed: bool,
    reporter: &dyn BatchReporter,
) -> AppResult<DeleteReport> {
    let paths = LibraryPaths::new(root);

    let selected = database.with_reader(|connection| {
        let mut selected = Vec::new();
        if let Some(ids) = image_ids {
            for chunk in ids.chunks(SQL_CHUNK) {
                let marks = placeholders(chunk.len());
                let mut statement = connection.prepare(&format!(
                    "SELECT images.id, images.content_hash, images.file_ext, folders.path
                     FROM images JOIN folders ON folders.id = images.folder_id
                     WHERE images.id IN ({marks})"
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
        } else if only_trashed {
            let mut statement = connection.prepare(
                "SELECT images.id, images.content_hash, images.file_ext, folders.path
                 FROM images JOIN folders ON folders.id = images.folder_id
                 WHERE images.deleted_at IS NOT NULL",
            )?;
            selected.extend(
                statement
                    .query_map([], |row| {
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
    for (image_id, content_hash, file_ext, folder_path) in selected {
        match content_path(&paths, &folder_path, &content_hash, &file_ext) {
            Ok(path) => files.push((image_id, path)),
            // 路径未通过校验时保留数据库行：此时无法定位原图，删行只会留下无人追踪的文件。
            // 正常导入的数据不会出现这种情况，告警用于暴露被外部改坏的库。
            Err(error) => log::warn!("跳过无效图片路径 image={image_id}: {error}"),
        }
    }

    let total = files.len();
    reporter.report(0, total);
    let mut report = DeleteReport::default();
    let mut done = 0_usize;

    for chunk in files.chunks(SQL_CHUNK) {
        if reporter.cancelled() {
            break;
        }
        let ids: Vec<i64> = chunk.iter().map(|(id, _)| *id).collect();
        let deleted = database.with_writer(|connection| {
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

        // 行已经删除；这一块的文件必须一并清理，取消只在块边界生效。
        for (image_id, path) in chunk {
            if path.exists() {
                match remove_content_file(&paths.root, path) {
                    Ok(()) => report.files_removed += 1,
                    Err(error) => report.failures.push(BatchFailure {
                        image_id: Some(*image_id),
                        path: path.to_string_lossy().into_owned(),
                        reason: error.to_string(),
                    }),
                }
            }
            done += 1;
            reporter.report(done, total);
        }
    }
    Ok(report)
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
    let normalized_folder = normalize_folder(folder_path)?;
    if normalized_folder != folder_path {
        return Err(AppError::message("文件夹路径未规范化，拒绝操作文件"));
    }
    let extension = normalize_content_extension(file_ext)?;
    if extension != file_ext {
        return Err(AppError::message("扩展名未规范化，拒绝操作文件"));
    }
    Ok(paths.content_file(&normalized_folder, &format!("{content_hash}.{extension}")))
}

fn ensure_inside_root(root: &Path, path: &Path) -> AppResult<()> {
    let canonical_root = root.canonicalize()?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::message("目标路径没有父目录"))?;
    let canonical_parent = parent.canonicalize()?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(AppError::message("目标路径位于图库目录之外"));
    }
    Ok(())
}

/// 目标目录必须已经存在，且 canonicalize 后仍是图库根目录或其子目录。
fn ensure_dir_inside_root(root: &Path, directory: &Path) -> AppResult<()> {
    let canonical_root = root.canonicalize()?;
    let canonical_directory = directory.canonicalize()?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(AppError::message("目标路径位于图库目录之外"));
    }
    Ok(())
}

pub(crate) fn remove_content_file(root: &Path, path: &Path) -> AppResult<()> {
    ensure_inside_root(root, path)?;
    let canonical_root = root.canonicalize()?;
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(&canonical_root) {
        return Err(AppError::message("文件位于图库目录之外"));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

fn tag_refs(database: &LibraryDb, image_id: i64) -> AppResult<Vec<TagRef>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT tags.id, tags.name
             FROM image_tags JOIN tags ON tags.id = image_tags.tag_id
             WHERE image_tags.image_id = ?1
             ORDER BY tags.name COLLATE NOCASE",
        )?;
        let tags = statement
            .query_map([image_id], |row| {
                Ok(TagRef {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(tags)
    })
}

fn tag_ids_for_names(connection: &Connection, names: &[String]) -> AppResult<Vec<i64>> {
    let mut ids = Vec::new();
    for name in names {
        let id: Option<i64> = connection
            .query_row(
                "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn normalize_tag_names(names: &[String]) -> AppResult<Vec<String>> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for name in names {
        let name = validate_tag_name(name)?;
        if seen.insert(name.to_lowercase()) {
            result.push(name);
        }
        if result.len() > MAX_TAGS_PER_IMAGE {
            return Err(AppError::message(format!(
                "每张图片最多 {MAX_TAGS_PER_IMAGE} 个标签"
            )));
        }
    }
    Ok(result)
}

fn validate_tag_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::message("标签名不能为空"));
    }
    if name.chars().count() > MAX_TAG_NAME_CHARS {
        return Err(AppError::message(format!(
            "标签名不能超过 {MAX_TAG_NAME_CHARS} 个字符"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(AppError::message("标签名包含控制字符"));
    }
    Ok(name.to_owned())
}

fn validate_score(score: i64) -> AppResult<()> {
    if !(0..=5).contains(&score) {
        return Err(AppError::message("评分必须在 0-5 之间"));
    }
    Ok(())
}

fn validate_ids(values: &[i64], max: usize) -> AppResult<Vec<i64>> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        if *value <= 0 {
            return Err(AppError::message("图片 id 无效"));
        }
        if seen.insert(*value) {
            result.push(*value);
        }
    }
    if result.is_empty() {
        return Err(AppError::message("没有选择图片"));
    }
    if result.len() > max {
        return Err(AppError::message(format!("一次最多操作 {max} 张图片")));
    }
    Ok(result)
}

fn validate_ids_for_files(values: &[i64]) -> AppResult<Vec<i64>> {
    validate_ids(values, MAX_FILE_BATCH_IDS).map_err(|error| match error {
        AppError::Message(message) if message.starts_with("一次最多操作") => {
            AppError::message(format!(
                "涉及文件的批量操作一次最多 {MAX_FILE_BATCH_IDS} 张，请分批执行"
            ))
        }
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::{NoopReporter, service};

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        root: PathBuf,
        folder: i64,
    }

    fn fixture() -> Fixture {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "元数据测试", 1).unwrap();
        let folder = session
            .db
            .with_writer(|connection| ensure_folder(connection, "人物", 1))
            .unwrap();
        Fixture {
            _temp: temp,
            session,
            root,
            folder,
        }
    }

    fn insert_image(
        fixture: &Fixture,
        seed: &str,
        extension: &str,
        score: i64,
        folder_id: i64,
    ) -> i64 {
        let hash = format!("{seed:0>64}");
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, ?2, ?3, 100, 10, 10, ?2, 0,
                        zeroblob(32), ?4, ?5, ?6, '', 1, 1)",
                    params![
                        hash,
                        extension,
                        folder_id,
                        vec![1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
                        vec![20_u8; 32],
                        score
                    ],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap()
    }

    fn write_content(
        fixture: &Fixture,
        folder_path: &str,
        hash_seed: &str,
        extension: &str,
    ) -> PathBuf {
        let hash = format!("{hash_seed:0>64}");
        let file_name = format!("{hash}.{extension}");
        let path = LibraryPaths::new(&fixture.root).content_file(folder_path, &file_name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"content").unwrap();
        path
    }

    #[test]
    fn reads_and_updates_metadata() {
        let fixture = fixture();
        let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
        assert_eq!(details.score, 1);
        assert_eq!(details.colors4.len(), 4);
        assert_eq!(details.colors4[0].weight, 4);
        assert_eq!(details.colors8.len(), 8);
        assert!(
            details
                .file_path
                .ends_with(&format!("{}.jpg", "0".repeat(63) + "a"))
        );

        update_score(&fixture.session.db, image_id, 5).unwrap();
        update_description(&fixture.session.db, image_id, "  人物速写  ").unwrap();

        let tags = set_image_tags(
            &fixture.session.db,
            image_id,
            &["速写".into(), "参考".into()],
        )
        .unwrap();
        assert_eq!(tags.len(), 2);

        let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
        assert_eq!(details.score, 5);
        assert_eq!(details.description, "人物速写");
        assert_eq!(details.description_source, "user");
        assert_eq!(details.tags.len(), 2);

        let found: i64 = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM image_search WHERE image_search MATCH '\"人物速\"'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(found, 1);
    }

    #[test]
    fn edits_tags_in_batches_and_renames() {
        let fixture = fixture();
        let first = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let second = insert_image(&fixture, "b", "png", 1, fixture.folder);

        assert_eq!(
            add_tags(
                &fixture.session.db,
                &[first, second],
                &["速写".into(), "练习".into()]
            )
            .unwrap(),
            4
        );
        assert_eq!(
            remove_tags(&fixture.session.db, &[first], &["练习".into()]).unwrap(),
            1
        );
        assert_eq!(
            image_details(&fixture.session.db, &fixture.root, first)
                .unwrap()
                .tags
                .len(),
            1
        );

        let tag_id = image_details(&fixture.session.db, &fixture.root, first)
            .unwrap()
            .tags[0]
            .id;
        rename_tag(&fixture.session.db, tag_id, "草稿").unwrap();
        let names: Vec<String> = image_details(&fixture.session.db, &fixture.root, second)
            .unwrap()
            .tags
            .into_iter()
            .map(|tag| tag.name)
            .collect();
        assert!(names.contains(&"草稿".to_owned()));

        let affected = delete_tag(&fixture.session.db, tag_id).unwrap();
        assert_eq!(affected, 2);
        assert_eq!(
            image_details(&fixture.session.db, &fixture.root, first)
                .unwrap()
                .tags
                .len(),
            0
        );
    }

    #[test]
    fn moves_files_between_folders_and_rolls_back_failures() {
        let fixture = fixture();
        let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let old_path = write_content(&fixture, "人物", "a", "jpg");

        let report = move_images(
            &fixture.session.db,
            &fixture.root,
            &[image_id],
            "风景/子目录",
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.moved, 1);
        assert!(report.failures.is_empty());
        assert!(!old_path.exists());
        let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
        assert_eq!(details.folder_path, "风景/子目录");
        assert!(Path::new(&details.file_path).exists());

        let report = move_images(
            &fixture.session.db,
            &fixture.root,
            &[image_id],
            "风景/子目录",
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.skipped, 1);

        // 目标位置被目录占用时，重命名失败；数据库仍指向原文件夹。
        let blocked = insert_image(&fixture, "b", "png", 1, fixture.folder);
        let blocked_path = write_content(&fixture, "人物", "b", "png");
        let target = LibraryPaths::new(&fixture.root)
            .content_file("风景/子目录", &format!("{}.png", "0".repeat(63) + "b"));
        fs::create_dir_all(&target).unwrap();
        let report = move_images(
            &fixture.session.db,
            &fixture.root,
            &[blocked],
            "风景/子目录",
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.moved, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(blocked_path.exists());
        assert_eq!(
            image_details(&fixture.session.db, &fixture.root, blocked)
                .unwrap()
                .folder_path,
            "人物"
        );
    }

    #[test]
    fn trashes_restores_and_permanently_deletes() {
        let fixture = fixture();
        let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let path = write_content(&fixture, "人物", "a", "jpg");
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute("INSERT INTO tags(name) VALUES ('速写')", [])?;
                connection.execute(
                    "INSERT INTO collections(id, name, sort_order, created_at, updated_at)
                     VALUES (1, '集合', 0, 1, 1)",
                    [],
                )?;
                connection.execute(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (1, ?1, 0)",
                    [image_id],
                )?;
                connection.execute(
                    "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name, pack_offset, pack_length, updated_at)
                     VALUES (?1, 'embedding', 'm', 3, 'f.bin', 0, 12, 1)",
                    [image_id],
                )?;
                connection.execute(
                    "INSERT INTO image_tags(image_id, tag_id)
                     SELECT ?1, id FROM tags WHERE name = '速写'",
                    [image_id],
                )?;
                Ok(())
            })
            .unwrap();

        assert_eq!(
            set_trashed(&fixture.session.db, &[image_id], true).unwrap(),
            1
        );
        let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
        assert!(details.deleted_at.is_some());
        assert!(path.exists());
        assert_eq!(
            set_trashed(&fixture.session.db, &[image_id], false).unwrap(),
            1
        );
        assert!(
            image_details(&fixture.session.db, &fixture.root, image_id)
                .unwrap()
                .deleted_at
                .is_none()
        );

        let report = delete_images(
            &fixture.session.db,
            &fixture.root,
            &[image_id],
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(report.files_removed, 1);
        assert!(!path.exists());
        assert!(image_details(&fixture.session.db, &fixture.root, image_id).is_err());

        let leftovers: (i64, i64, i64, i64) = fixture
            .session
            .db
            .with_reader(|connection| {
                Ok((
                    connection
                        .query_row("SELECT count(*) FROM image_tags", [], |row| row.get(0))?,
                    connection.query_row("SELECT count(*) FROM collection_items", [], |row| {
                        row.get(0)
                    })?,
                    connection
                        .query_row("SELECT count(*) FROM feature_blobs", [], |row| row.get(0))?,
                    connection
                        .query_row("SELECT count(*) FROM image_search", [], |row| row.get(0))?,
                ))
            })
            .unwrap();
        assert_eq!(leftovers, (0, 0, 0, 0));
    }

    #[test]
    fn empties_trash_only() {
        let fixture = fixture();
        let active = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        let trashed = insert_image(&fixture, "b", "jpg", 1, fixture.folder);
        write_content(&fixture, "人物", "a", "jpg");
        write_content(&fixture, "人物", "b", "jpg");
        set_trashed(&fixture.session.db, &[trashed], true).unwrap();

        let report = empty_trash(&fixture.session.db, &fixture.root, &NoopReporter).unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(report.files_removed, 1);
        assert!(image_details(&fixture.session.db, &fixture.root, active).is_ok());
        assert!(image_details(&fixture.session.db, &fixture.root, trashed).is_err());
    }

    #[test]
    fn rejects_invalid_inputs() {
        let fixture = fixture();
        let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
        assert!(update_score(&fixture.session.db, image_id, 6).is_err());
        assert!(update_description(&fixture.session.db, image_id, &"x".repeat(50_001)).is_err());
        assert!(rename_tag(&fixture.session.db, 1, "").is_err());
        assert!(
            move_images(
                &fixture.session.db,
                &fixture.root,
                &[],
                "目标",
                &NoopReporter
            )
            .is_err()
        );
        assert!(
            move_images(
                &fixture.session.db,
                &fixture.root,
                &[image_id],
                "../外面",
                &NoopReporter
            )
            .is_err()
        );
    }
}

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        paths::{escape_like, join_folder, normalize_folder, parent_path},
        thumbs::ThumbLocation,
    },
};

pub struct ImageInsert {
    pub content_hash: String,
    pub file_ext: String,
    pub file_size: u64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub has_alpha: bool,
    pub phash: [u8; 32],
    pub colors4: [u8; 16],
    pub colors8: [u8; 32],
    pub score: u8,
    pub thumb: ThumbLocation,
    pub thumb_format: String,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub imported_at: i64,
    /// 导入时压缩过就写当前时间；以后跑全库压缩会跳过这些行。
    pub optimized_at: i64,
    /// 导入记录 id；撤销导入时按它找回本次新增的行。
    pub origin_operation_id: Option<i64>,
}

/// 创建文件夹行以及全部缺失祖先；路径按 Windows 大小写语义解析。
///
/// 生产写路径用带追踪的版本；这个简化包装只给测试准备固定目录。
#[cfg(test)]
pub fn ensure_folder(connection: &Connection, path: &str, now: i64) -> AppResult<i64> {
    ensure_folder_tracked(connection, path, now, &mut Vec::new())
}

/// 与 `ensure_folder` 相同，但把本次真正新建的文件夹 `(id, path)` 追加到 `created`。
pub fn ensure_folder_tracked(
    connection: &Connection,
    path: &str,
    now: i64,
    created: &mut Vec<(i64, String)>,
) -> AppResult<i64> {
    ensure_folder_path(connection, path, now, created).map(|(id, _)| id)
}

/// 与 `ensure_folder_tracked` 相同，并返回数据库里的规范路径。
///
/// 已存在的祖先可能以另一种大小写存着（例如库里是 `Art`、请求是 `art/sub`）；
/// 子路径必须拼在规范路径后面，否则同一棵子树会出现两种前缀写法，
/// 按路径前缀聚合的计数与祖先/子树判断都会漏掉它。
pub fn ensure_folder_path(
    connection: &Connection,
    path: &str,
    now: i64,
    created: &mut Vec<(i64, String)>,
) -> AppResult<(i64, String)> {
    let path = normalize_folder(path)?;
    let (mut id, mut current) = resolve_folder(connection, "", now, created)?;
    if path.is_empty() {
        return Ok((id, current));
    }
    for segment in path.split('/') {
        let candidate = join_folder(&current, segment);
        let (next_id, canonical) = resolve_folder(connection, &candidate, now, created)?;
        id = next_id;
        current = canonical;
    }
    Ok((id, current))
}

/// 解析单个路径：已存在就返回库里那一行的路径，不存在就按传入路径新建。
fn resolve_folder(
    connection: &Connection,
    path: &str,
    now: i64,
    created: &mut Vec<(i64, String)>,
) -> AppResult<(i64, String)> {
    if let Some((id, stored_path, trashed_at, purge_operation_id)) = connection
        .query_row(
            "SELECT id, path, trashed_at, purge_operation_id FROM folders
             WHERE path = ?1 COLLATE NOCASE",
            [path],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?
    {
        if purge_operation_id.is_some() {
            return Err(AppError::message(format!(
                "文件夹「{path}」正在释放，请稍后再试"
            )));
        }
        if trashed_at.is_some() {
            return Err(AppError::message(format!(
                "文件夹「{path}」在回收站中，请先恢复或永久删除"
            )));
        }
        return Ok((id, stored_path));
    }
    let sort_index = next_sort_index(connection, path)?;
    connection.execute(
        "INSERT INTO folders(path, created_at, sort_index) VALUES (?1, ?2, ?3)",
        params![path, now, sort_index],
    )?;
    let id = connection.last_insert_rowid();
    created.push((id, path.to_owned()));
    Ok((id, path.to_owned()))
}

/// 新文件夹追加到同一父级的末尾，避免手动排序里新文件夹与已有项并列 0。
pub(crate) fn next_sort_index(connection: &Connection, path: &str) -> AppResult<i64> {
    let parent = parent_path(path);
    if parent.is_empty() {
        connection
            .query_row(
                "SELECT COALESCE(MAX(sort_index), -1) + 1 FROM folders
                 WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
                   AND path <> '' AND instr(path, '/') = 0",
                [],
                |row| row.get(0),
            )
            .map_err(AppError::from)
    } else {
        connection
            .query_row(
                "SELECT COALESCE(MAX(sort_index), -1) + 1 FROM folders
                 WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
                   AND path LIKE ?1 ESCAPE '\\'
                   AND path <> ?2
                   AND instr(substr(path, length(?2) + 2), '/') = 0",
                params![format!("{}/%", escape_like(parent)), parent],
                |row| row.get(0),
            )
            .map_err(AppError::from)
    }
}

/// 确保标签存在，并把本次真正新建的标签 `(id, name)` 追加到 `created`。
pub fn ensure_tags_tracked(
    connection: &Connection,
    tags: &[String],
    created: &mut Vec<(i64, String)>,
) -> AppResult<Vec<i64>> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for tag in tags {
        let name = tag.trim();
        if name.is_empty() || !seen.insert(name.to_lowercase()) {
            continue;
        }
        let inserted = connection.execute(
            "INSERT INTO tags(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            [name],
        )?;
        let id: i64 = connection.query_row(
            "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
            [name],
            |row| row.get(0),
        )?;
        if inserted > 0 {
            created.push((id, name.to_owned()));
        }
        ids.push(id);
    }
    Ok(ids)
}

/// 插入一行；`content_hash` 已存在时返回 `None`，由调用方按重复策略处理。
pub fn insert_image(
    connection: &Connection,
    folder_id: i64,
    image: &ImageInsert,
) -> AppResult<Option<i64>> {
    let file_size = i64::try_from(image.file_size)
        .map_err(|_| AppError::message("图片文件大小超出数据库范围"))?;
    let thumb_offset = i64::try_from(image.thumb.offset)
        .map_err(|_| AppError::message("缩略图偏移超出数据库范围"))?;

    let changed = connection.execute(
        "INSERT INTO images(
            content_hash, file_ext, folder_id, file_size, width, height, format,
            has_alpha, phash, colors4, colors8, score, description, imported_at, modified_at,
            thumb_segment, thumb_offset, thumb_length, thumb_format,
            thumb_width, thumb_height, thumb_version, optimized_at, origin_operation_id
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, '', ?13, ?13,
            ?14, ?15, ?16, ?17, ?18, ?19, 1, ?20, ?21
         )
         ON CONFLICT(content_hash) DO NOTHING",
        params![
            image.content_hash,
            image.file_ext,
            folder_id,
            file_size,
            image.width,
            image.height,
            image.format,
            image.has_alpha,
            image.phash.as_slice(),
            image.colors4.as_slice(),
            image.colors8.as_slice(),
            image.score,
            image.imported_at,
            image.thumb.segment,
            thumb_offset,
            image.thumb.length,
            image.thumb_format,
            image.thumb_width,
            image.thumb_height,
            image.optimized_at,
            image.origin_operation_id,
        ],
    )?;

    if changed == 0 {
        return Ok(None);
    }
    Ok(Some(connection.last_insert_rowid()))
}

pub fn attach_tags(connection: &Connection, image_id: i64, tag_ids: &[i64]) -> AppResult<()> {
    for tag_id in tag_ids {
        connection.execute(
            "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)
             ON CONFLICT(image_id, tag_id) DO NOTHING",
            params![image_id, tag_id],
        )?;
    }
    Ok(())
}

/// 按内容哈希找已有图片，同时返回它是否在回收站（待释放行也算已存在）。
///
/// `include_temp = false` 时忽略临时文件夹里的行：图库导入要真正入库，不能把
/// 「临时区看过一眼的同内容图」当成已存在的库内图片。
pub fn find_by_hash_state(
    connection: &Connection,
    hash: &str,
    include_temp: bool,
) -> AppResult<Option<(i64, bool)>> {
    // 排除临时行直接复用 `temp::EXCLUDE_SQL`，避免两处口径漂移。
    let filter = if include_temp {
        String::new()
    } else {
        format!(" AND {}", crate::library::temp::EXCLUDE_SQL)
    };
    connection
        .query_row(
            &format!(
                "SELECT id, deleted_at IS NOT NULL FROM images
                 WHERE content_hash = ?1{filter}"
            ),
            [hash],
            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
}

/// 丢弃占着这个内容哈希的临时行，把唯一约束让给图库导入。
///
/// 临时区是「只看不留」的暂存，关闭图库即清空；图库导入命中同内容时如果不让出
/// 这一行，内容哈希的唯一约束会让这张图永远进不了图库，而临时区里的 id 又不能移动、
/// 不能入集合、没有删除入口。返回临时目录里的原图文件名，由调用方在提交后清理。
pub fn discard_temporary_duplicate(
    connection: &Connection,
    hash: &str,
) -> AppResult<Option<String>> {
    let row = connection
        .query_row(
            "SELECT images.id, images.content_hash, images.file_ext
             FROM images JOIN folders ON folders.id = images.folder_id
             WHERE images.content_hash = ?1 AND folders.kind = 'temp'",
            [hash],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((image_id, content_hash, file_ext)) = row else {
        return Ok(None);
    };
    connection.execute("DELETE FROM images WHERE id = ?1", [image_id])?;
    Ok(Some(format!("{content_hash}.{file_ext}")))
}

/// 合并重复项：评分取较高值，标签取并集，不改描述与收藏状态（后续阶段接入）。
pub fn merge_metadata(
    connection: &Connection,
    image_id: i64,
    score: u8,
    tag_ids: &[i64],
    now: i64,
) -> AppResult<()> {
    connection.execute(
        "UPDATE images SET score = max(score, ?1), modified_at = ?2 WHERE id = ?3",
        params![score, now, image_id],
    )?;
    attach_tags(connection, image_id, tag_ids)
}

pub fn ensure_thumb_segment(connection: &Connection, segment: u32, now: i64) -> AppResult<()> {
    let file_name = format!("{segment:04}.pack");
    connection.execute(
        "INSERT INTO thumb_segments(id, file_name, size, created_at) VALUES (?1, ?2, 0, ?3)
         ON CONFLICT(id) DO NOTHING",
        params![segment, file_name, now],
    )?;
    Ok(())
}

pub fn update_thumb_segment_size(
    connection: &Connection,
    segment: u32,
    size: u64,
) -> AppResult<()> {
    let size = i64::try_from(size).unwrap_or(i64::MAX);
    connection.execute(
        "UPDATE thumb_segments SET size = ?1 WHERE id = ?2",
        params![size, segment],
    )?;
    Ok(())
}

/// 库内 pHash 索引；临时区的行不算库内图片，不参与“疑似重复”提示。
pub fn load_phashes(connection: &Connection) -> AppResult<Vec<(i64, [u8; 32])>> {
    let mut statement = connection.prepare(&format!(
        "SELECT id, phash FROM images
         WHERE deleted_at IS NULL AND length(phash) = 32 AND {}",
        crate::library::temp::EXCLUDE_SQL
    ))?;
    let rows = statement.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        let mut hash = [0_u8; 32];
        hash.copy_from_slice(&bytes);
        Ok((id, hash))
    })?;
    let mut hashes = Vec::new();
    for row in rows {
        hashes.push(row?);
    }
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::library::{service, thumbs::ThumbLocation};

    fn sample_hash(seed: u8) -> String {
        format!("{seed:02x}").repeat(32)
    }

    #[test]
    fn inserts_and_merges_by_content_hash() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "测试", 1).unwrap();
        let hash = sample_hash(1);

        let inserted = session
            .db
            .with_writer(|connection| {
                let folder = ensure_folder(connection, "人物", 1)?;
                let tags = ensure_tags_tracked(
                    connection,
                    &["速写".into(), "参考".into()],
                    &mut Vec::new(),
                )?;
                let id = insert_image(
                    connection,
                    folder,
                    &ImageInsert {
                        content_hash: hash.clone(),
                        file_ext: "png".into(),
                        file_size: 10,
                        width: 4,
                        height: 3,
                        format: "png".into(),
                        has_alpha: false,
                        phash: [7; 32],
                        colors4: [8; 16],
                        colors8: [9; 32],
                        score: 2,
                        thumb: ThumbLocation {
                            segment: 1,
                            offset: 16,
                            length: 4,
                        },
                        thumb_format: "webp".into(),
                        thumb_width: 4,
                        thumb_height: 3,
                        imported_at: 1,
                        optimized_at: 0,
                        origin_operation_id: None,
                    },
                )?;
                attach_tags(connection, id.unwrap(), &tags)?;
                Ok(id)
            })
            .unwrap();
        assert!(inserted.is_some());

        session
            .db
            .with_writer(|connection| {
                let tags = ensure_tags_tracked(
                    connection,
                    &["速写".into(), "新标签".into()],
                    &mut Vec::new(),
                )?;
                assert!(
                    insert_image(
                        connection,
                        ensure_folder(connection, "人物", 2)?,
                        &ImageInsert {
                            content_hash: hash.clone(),
                            file_ext: "png".into(),
                            file_size: 10,
                            width: 4,
                            height: 3,
                            format: "png".into(),
                            has_alpha: false,
                            phash: [7; 32],
                            colors4: [8; 16],
                            colors8: [9; 32],
                            score: 4,
                            thumb: ThumbLocation {
                                segment: 1,
                                offset: 16,
                                length: 4,
                            },
                            thumb_format: "webp".into(),
                            thumb_width: 4,
                            thumb_height: 3,
                            imported_at: 2,
                            optimized_at: 0,
                            origin_operation_id: None,
                        },
                    )?
                    .is_none()
                );
                let existing = find_by_hash_state(connection, &hash, true)?.unwrap().0;
                merge_metadata(connection, existing, 4, &tags, 3)?;
                Ok(())
            })
            .unwrap();

        session
            .db
            .with_reader(|connection| {
                let (score, tag_count): (i64, i64) = connection.query_row(
                    "SELECT score,
                            (SELECT count(*) FROM image_tags WHERE image_id = images.id)
                     FROM images WHERE content_hash = ?1",
                    [&hash],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(score, 4);
                assert_eq!(tag_count, 3);
                Ok(())
            })
            .unwrap();

        session
            .db
            .with_reader(|connection| {
                let hashes = load_phashes(connection)?;
                assert_eq!(hashes.len(), 1);
                assert_eq!(hashes[0].1, [7; 32]);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn temp_row_is_invisible_to_library_dedup_and_yields_the_hash() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "测试", 1).unwrap();
        let hash = sample_hash(3);
        let temp_folder = session
            .db
            .with_reader(crate::library::temp::folder)
            .unwrap()
            .unwrap();

        let insert = |seed: u8| ImageInsert {
            content_hash: hash.clone(),
            file_ext: "png".into(),
            file_size: 10,
            width: 4,
            height: 3,
            format: "png".into(),
            has_alpha: false,
            phash: [seed; 32],
            colors4: [seed; 16],
            colors8: [seed; 32],
            score: 0,
            thumb: ThumbLocation {
                segment: 1,
                offset: 16,
                length: 4,
            },
            thumb_format: "webp".into(),
            thumb_width: 4,
            thumb_height: 3,
            imported_at: 1,
            optimized_at: 0,
            origin_operation_id: None,
        };

        let temp_image = session
            .db
            .with_writer(|connection| insert_image(connection, temp_folder.id, &insert(1)))
            .unwrap()
            .unwrap();

        // 临时行对图库导入不可见，但临时导入仍然看得到它。
        session
            .db
            .with_reader(|connection| {
                assert!(find_by_hash_state(connection, &hash, false)?.is_none());
                assert_eq!(
                    find_by_hash_state(connection, &hash, true)?.map(|item| item.0),
                    Some(temp_image)
                );
                Ok(())
            })
            .unwrap();

        session
            .db
            .with_writer(|connection| {
                let discarded = discard_temporary_duplicate(connection, &hash)?.unwrap();
                assert_eq!(discarded, format!("{hash}.png"));
                let left: i64 = connection.query_row(
                    "SELECT count(*) FROM images WHERE id = ?1",
                    [temp_image],
                    |row| row.get(0),
                )?;
                assert_eq!(left, 0, "临时行应当被删掉");
                // 让出唯一约束后，图库行才能真正插入。
                let folder = ensure_folder(connection, "人物", 2)?;
                assert!(insert_image(connection, folder, &insert(2))?.is_some());
                Ok(())
            })
            .unwrap();

        session
            .db
            .with_reader(|connection| {
                // 图库行拿到的是新的 rowid；这里只关心它确实存在且不再是临时行。
                assert!(find_by_hash_state(connection, &hash, false)?.is_some());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn child_paths_follow_the_stored_parent_case() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "测试", 1).unwrap();
        session
            .db
            .with_writer(|connection| {
                let mut created = Vec::new();
                let (_, parent_path) = ensure_folder_path(connection, "Art", 1, &mut created)?;
                assert_eq!(parent_path, "Art");
                assert_eq!(created.len(), 1);

                // 子路径拼在库里的规范路径后面，不再沿用调用方的大小写。
                let (child, child_path) =
                    ensure_folder_path(connection, "art/sub", 2, &mut created)?;
                assert_eq!(child_path, "Art/sub");
                let stored: String = connection.query_row(
                    "SELECT path FROM folders WHERE id = ?1",
                    [child],
                    |row| row.get(0),
                )?;
                assert_eq!(stored, "Art/sub");

                // 已存在的路径原样返回，不新建行。
                let (again, again_path) =
                    ensure_folder_path(connection, "ART/SUB", 3, &mut created)?;
                assert_eq!(again, child);
                assert_eq!(again_path, "Art/sub");
                assert_eq!(created.len(), 2);
                Ok(())
            })
            .unwrap();
    }
}

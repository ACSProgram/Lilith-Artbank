use std::collections::HashSet;

use rusqlite::{Connection, params};

use crate::{
    app::error::{AppError, AppResult},
    library::thumbs::ThumbLocation,
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
}

pub fn ensure_folder(connection: &Connection, path: &str, now: i64) -> AppResult<i64> {
    connection.execute(
        "INSERT INTO folders(path, created_at) VALUES (?1, ?2)
         ON CONFLICT(path) DO NOTHING",
        params![path, now],
    )?;
    let id = connection.query_row("SELECT id FROM folders WHERE path = ?1", [path], |row| {
        row.get(0)
    })?;
    Ok(id)
}

pub fn ensure_tags(connection: &Connection, tags: &[String]) -> AppResult<Vec<i64>> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for tag in tags {
        let name = tag.trim();
        if name.is_empty() || !seen.insert(name.to_lowercase()) {
            continue;
        }
        connection.execute(
            "INSERT INTO tags(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            [name],
        )?;
        let id: i64 = connection.query_row(
            "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
            [name],
            |row| row.get(0),
        )?;
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
            thumb_width, thumb_height, thumb_version
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, '', ?13, ?13,
            ?14, ?15, ?16, ?17, ?18, ?19, 1
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

pub fn find_by_hash(connection: &Connection, hash: &str) -> AppResult<Option<i64>> {
    connection
        .query_row(
            "SELECT id FROM images WHERE content_hash = ?1",
            [hash],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
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

pub fn load_phashes(connection: &Connection) -> AppResult<Vec<(i64, [u8; 32])>> {
    let mut statement = connection
        .prepare("SELECT id, phash FROM images WHERE deleted_at IS NULL AND length(phash) = 32")?;
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
                let tags = ensure_tags(connection, &["速写".into(), "参考".into()])?;
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
                let tags = ensure_tags(connection, &["速写".into(), "新标签".into()])?;
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
                        },
                    )?
                    .is_none()
                );
                let existing = find_by_hash(connection, &hash)?.unwrap();
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
}

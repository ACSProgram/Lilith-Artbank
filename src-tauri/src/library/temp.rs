//! 临时文件夹：每个图库唯一的 `folders.kind = 'temp'` 目录。
//!
//! 临时图片只能通过导入进入，不参与默认计数、默认搜索与文件夹树；打开、
//! 关闭、切换图库或退出应用时整目录清空。临时图片的元数据修改不写操作记录。

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        paths::{LibraryPaths, TEMP_FOLDER_DIR},
        placeholders,
    },
};

/// 排除临时图片的 SQL 条件；依赖 `images` 表名（不要改写别名）。
///
/// 必须用标量子查询而不是 `NOT IN (子查询)`：后者在 SQLite 里按行探测临时索引，
/// 20 万行建文件夹排序 id 列表会从 41 ms 涨到 54 ms，标量子查询只求值一次。
/// `COALESCE` 兜住“还没有临时文件夹行”的旧库，否则 `<> NULL` 会把整库都过滤掉。
pub const EXCLUDE_SQL: &str = "images.folder_id <> COALESCE((SELECT temp_folders.id FROM folders temp_folders WHERE temp_folders.kind = 'temp'), -1)";

/// 只取临时图片的 SQL 条件；与 [`EXCLUDE_SQL`] 同一写法。
pub const ONLY_SQL: &str = "images.folder_id = COALESCE((SELECT temp_folders.id FROM folders temp_folders WHERE temp_folders.kind = 'temp'), -1)";

#[derive(Debug, Clone)]
pub struct TempFolder {
    pub id: i64,
    pub path: String,
}

pub fn folder(connection: &Connection) -> AppResult<Option<TempFolder>> {
    Ok(connection
        .query_row(
            "SELECT id, path FROM folders WHERE kind = 'temp'",
            [],
            |row| {
                Ok(TempFolder {
                    id: row.get(0)?,
                    path: row.get(1)?,
                })
            },
        )
        .optional()?)
}

pub fn require(connection: &Connection) -> AppResult<TempFolder> {
    folder(connection)?.ok_or_else(|| AppError::message("临时文件夹不存在，请重新打开图库"))
}

pub fn path(connection: &Connection) -> AppResult<Option<String>> {
    Ok(folder(connection)?.map(|item| item.path))
}

pub fn is_image(database: &LibraryDb, image_id: i64) -> AppResult<bool> {
    database.with_reader(|connection| is_image_in(connection, image_id))
}

pub fn is_image_in(connection: &Connection, image_id: i64) -> AppResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM images JOIN folders ON folders.id = images.folder_id
             WHERE images.id = ?1 AND folders.kind = 'temp'
         )",
        [image_id],
        |row| row.get(0),
    )?)
}

/// 在给定 id 里找出位于临时文件夹的存在行。
pub fn image_ids_in(connection: &Connection, image_ids: &[i64]) -> AppResult<HashSet<i64>> {
    let mut ids = HashSet::new();
    for chunk in image_ids.chunks(400) {
        let marks = placeholders(chunk.len());
        let mut statement = connection.prepare(&format!(
            "SELECT images.id FROM images JOIN folders ON folders.id = images.folder_id
             WHERE images.id IN ({marks}) AND folders.kind = 'temp'"
        ))?;
        let rows = statement.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            row.get::<_, i64>(0)
        })?;
        for row in rows {
            ids.insert(row?);
        }
    }
    Ok(ids)
}

pub fn ids_in(database: &LibraryDb, image_ids: &[i64]) -> AppResult<HashSet<i64>> {
    database.with_reader(|connection| image_ids_in(connection, image_ids))
}

/// 写入前置检查：临时图片不能移入回收站/恢复/永久删除、移动、编辑或加入集合。
pub fn reject_images(database: &LibraryDb, image_ids: &[i64], message: &str) -> AppResult<()> {
    if !ids_in(database, image_ids)?.is_empty() {
        return Err(AppError::message(message.to_owned()));
    }
    Ok(())
}

pub fn reject_images_in(
    connection: &Connection,
    image_ids: &[i64],
    message: &str,
) -> AppResult<()> {
    if !image_ids_in(connection, image_ids)?.is_empty() {
        return Err(AppError::message(message.to_owned()));
    }
    Ok(())
}

/// 打开或创建图库后确保行与物理目录都存在；旧库迁移通常已经补好行。
pub fn ensure(database: &LibraryDb, root: &Path) -> AppResult<TempFolder> {
    let folder = database.with_writer(|connection| {
        if let Some(folder) = folder(connection)? {
            return Ok(folder);
        }
        let path = free_path(connection)?;
        connection.execute(
            "INSERT INTO folders(path, created_at, sort_index, kind)
             VALUES (?1, ?2, -1, 'temp')",
            params![path, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(TempFolder {
            id: connection.last_insert_rowid(),
            path,
        })
    })?;
    std::fs::create_dir_all(temp_dir(root, &folder.path))?;
    Ok(folder)
}

fn temp_dir(root: &Path, path: &str) -> PathBuf {
    LibraryPaths::new(root).folder_dir(path)
}

/// 找一个没被占用的临时目录名：默认 `.artbank-temp`，被普通文件夹占用时追加 `-1`、`-2`。
///
/// v6→v7 迁移与 [`ensure`] 共用；迁移时迁移器还没有 `LibraryDb`，只能传连接。
pub(crate) fn free_path(connection: &Connection) -> AppResult<String> {
    for suffix in 0..1000 {
        let candidate = if suffix == 0 {
            TEMP_FOLDER_DIR.to_owned()
        } else {
            format!("{TEMP_FOLDER_DIR}-{suffix}")
        };
        let exists: bool = connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM folders WHERE path = ?1 COLLATE NOCASE)",
            [&candidate],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(candidate);
        }
    }
    Err(AppError::message("无法为临时文件夹找到可用目录名"))
}

/// 清空临时文件夹：删行、删原图与目录内容，但保留临时文件夹本身。
///
/// 临时导入固定平铺，导入器也不会在临时文件夹下建子目录，因此按 `folder_id` 删行就够；
/// `image_tags` / `collection_items` / `feature_blobs` / FTS 都由外键级联与触发器跟着删。
pub fn clear(database: &LibraryDb, root: &Path) -> AppResult<u64> {
    let Some(folder) = database.with_reader(folder)? else {
        return Ok(0);
    };
    let directory = temp_dir(root, &folder.path);
    let removed = database.with_writer(|connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let removed =
            transaction.execute("DELETE FROM images WHERE folder_id = ?1", [folder.id])?;
        transaction.commit()?;
        Ok(removed as u64)
    })?;
    if directory.exists()
        && let Err(error) = std::fs::remove_dir_all(&directory)
    {
        log::warn!(
            "清空临时文件夹失败：{error}；目录位于 {}",
            directory.display()
        );
    }
    if let Err(error) = std::fs::create_dir_all(&directory) {
        log::warn!(
            "重建临时文件夹失败：{error}；目录位于 {}",
            directory.display()
        );
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::library::{import::ImageInsert, service, thumbs::ThumbLocation};

    fn fixture() -> (tempfile::TempDir, service::LibrarySession, PathBuf) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "临时测试", 2).unwrap();
        (temp, session, root)
    }

    fn insert(session: &service::LibrarySession, folder_id: i64, seed: u8) -> i64 {
        let hash = format!("{seed:02x}").repeat(32);
        session
            .db
            .with_writer(|connection| {
                let id = super::super::import::insert_image(
                    connection,
                    folder_id,
                    &ImageInsert {
                        content_hash: hash.clone(),
                        file_ext: "jpg".into(),
                        file_size: 3,
                        width: 2,
                        height: 2,
                        format: "jpeg".into(),
                        has_alpha: false,
                        phash: [seed; 32],
                        colors4: [seed; 16],
                        colors8: [seed; 32],
                        score: 0,
                        thumb: ThumbLocation {
                            segment: 1,
                            offset: 0,
                            length: 2,
                        },
                        thumb_format: "webp".into(),
                        thumb_width: 2,
                        thumb_height: 2,
                        imported_at: 1,
                        optimized_at: 0,
                        origin_operation_id: None,
                    },
                )?;
                Ok(id.unwrap())
            })
            .unwrap()
    }

    #[test]
    fn new_library_has_exactly_one_hidden_temp_folder() {
        let (_temp, session, root) = fixture();
        let temp = session.db.with_reader(folder).unwrap().unwrap();
        assert_eq!(temp.path, TEMP_FOLDER_DIR);
        assert!(root.join(TEMP_FOLDER_DIR).is_dir());
        let listed = super::super::query::list_folders(&session.db).unwrap();
        assert!(listed.iter().all(|item| item.path != temp.path));
    }

    #[test]
    fn clears_rows_files_and_collection_items() {
        let (_temp, session, root) = fixture();
        let temp = session.db.with_reader(folder).unwrap().unwrap();
        let normal = session
            .db
            .with_writer(|connection| super::super::import::ensure_folder(connection, "作品", 1))
            .unwrap();
        let temp_image = insert(&session, temp.id, 1);
        let normal_image = insert(&session, normal, 2);
        let file = root
            .join(TEMP_FOLDER_DIR)
            .join(format!("{}.jpg", "01".repeat(32)));
        std::fs::write(&file, b"x").unwrap();
        let collection = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO collections(name, sort_order, created_at, updated_at)
                     VALUES ('测试', 0, 1, 1)",
                    [],
                )?;
                let id = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (?1, ?2, 0)",
                    [id, temp_image],
                )?;
                Ok(id)
            })
            .unwrap();

        let removed = clear(&session.db, &root).unwrap();
        assert_eq!(removed, 1);
        assert!(!file.exists());
        assert!(root.join(TEMP_FOLDER_DIR).is_dir());
        let counts: (i64, i64, i64) = session
            .db
            .with_reader(|connection| {
                Ok((
                    connection.query_row(
                        "SELECT count(*) FROM images WHERE id = ?1",
                        [temp_image],
                        |row| row.get(0),
                    )?,
                    connection.query_row(
                        "SELECT count(*) FROM images WHERE id = ?1",
                        [normal_image],
                        |row| row.get(0),
                    )?,
                    connection.query_row(
                        "SELECT count(*) FROM collection_items WHERE collection_id = ?1",
                        [collection],
                        |row| row.get(0),
                    )?,
                ))
            })
            .unwrap();
        assert_eq!(counts, (0, 1, 0));
    }

    #[test]
    fn rejects_temp_images_for_forbidden_writes() {
        let (_temp, session, _root) = fixture();
        let temp = session.db.with_reader(folder).unwrap().unwrap();
        let image = insert(&session, temp.id, 3);
        assert!(reject_images(&session.db, &[image], "不允许").is_err());
        let ids = session
            .db
            .with_reader(|connection| image_ids_in(connection, &[image]))
            .unwrap();
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), vec![image]);
    }
}

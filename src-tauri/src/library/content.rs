use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use crate::{
    app::error::{AppError, AppResult},
    concurrency::resources::{HolderInfo, ResourceRequest, ResourceTable, WaitError},
    library::{db::LibraryDb, metadata, paths::LibraryPaths},
};

/// 查看器与编辑保存共用的原图行快照。
///
/// `phash` / `colors*` / `modified_at` 只用于编辑撤销的前置快照；查看器路径不读它们。
pub struct ContentRow {
    pub id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub folder_path: String,
    pub file_size: i64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub has_alpha: bool,
    pub phash: Vec<u8>,
    pub colors4: Vec<u8>,
    pub colors8: Vec<u8>,
    pub modified_at: i64,
    pub thumb_version: i64,
    pub deleted_at: Option<i64>,
}

impl ContentRow {
    pub fn pixel_count(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    pub fn path(&self, paths: &LibraryPaths) -> AppResult<PathBuf> {
        metadata::content_path(paths, &self.folder_path, &self.content_hash, &self.file_ext)
    }
}

/// 流式计算文件的 BLAKE3。
///
/// 只用于少见路径：发布时目标已存在、需要判断它是完整内容还是上次崩溃留下的
/// 孤儿；正常发布不会调用。
pub(crate) fn hash_file(path: &Path) -> AppResult<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// 拿到原图所在文件夹的读锁，并返回锁内重新解析出的最新行。
///
/// 先读一次行 → 锁该文件夹 → 再读一次；两次一致才算有效。这样文件夹改名/图片移动
/// 恰好发生在两次读取之间时，会先释放再按新路径重试，而不是拿旧路径的锁读新行。
pub fn lock_original(
    database: &LibraryDb,
    resources: &ResourceTable,
    image_id: i64,
    holder: &HolderInfo,
    cancel: &dyn Fn() -> bool,
) -> AppResult<(ContentRow, crate::concurrency::resources::ResourceGuard)> {
    for _ in 0..3 {
        let row = load(database, image_id)?;
        let guard = resources
            .acquire_path_read(&row.folder_path, holder, cancel)
            .map_err(|error| match error {
                WaitError::Cancelled => AppError::message("读取已取消"),
                WaitError::Timeout(_) => AppError::message("原图所在文件夹正忙，请稍后重试"),
            })?;
        if let Some(latest) = verified(database, &row)? {
            return Ok((latest, guard));
        }
        drop(guard);
    }
    Err(AppError::message("原图所在文件夹正在被修改，请稍后重试"))
}

/// 尽力而为版本：文件夹正被改写时不等待，直接返回可读错误。
/// 查看器、MCP 拼图与复制这类交互式读取用它，避免被长时间压缩任务挂住。
pub fn try_lock_original(
    database: &LibraryDb,
    resources: &ResourceTable,
    image_id: i64,
    holder: &HolderInfo,
) -> AppResult<(ContentRow, crate::concurrency::resources::ResourceGuard)> {
    for _ in 0..3 {
        let row = load(database, image_id)?;
        let guard = resources
            .try_acquire(&ResourceRequest::path_read(&row.folder_path), holder)
            .map_err(|_| AppError::message("原图所在文件夹正忙，请稍后重试"))?;
        if let Some(latest) = verified(database, &row)? {
            return Ok((latest, guard));
        }
        drop(guard);
    }
    Err(AppError::message("原图所在文件夹正在被修改，请稍后重试"))
}

fn verified(database: &LibraryDb, row: &ContentRow) -> AppResult<Option<ContentRow>> {
    let latest = load(database, row.id)?;
    let same_location = latest.folder_path.eq_ignore_ascii_case(&row.folder_path)
        && latest.content_hash == row.content_hash;
    Ok(same_location.then_some(latest))
}

/// 在文件夹读锁保护下按 id 重新解析并读取原图字节；路径被改名时重试。
pub fn read_original(
    database: &LibraryDb,
    resources: &ResourceTable,
    root: &Path,
    image_id: i64,
    holder: &HolderInfo,
    cancel: &dyn Fn() -> bool,
) -> AppResult<(ContentRow, Vec<u8>)> {
    let (row, guard) = lock_original(database, resources, image_id, holder, cancel)?;
    let bytes = std::fs::read(row.path(&LibraryPaths::new(root))?)?;
    drop(guard);
    Ok((row, bytes))
}

/// `read_original` 的尽力而为版本；文件夹被改写时不等待。
pub fn try_read_original(
    database: &LibraryDb,
    resources: &ResourceTable,
    root: &Path,
    image_id: i64,
    holder: &HolderInfo,
) -> AppResult<(ContentRow, Vec<u8>)> {
    let (row, guard) = try_lock_original(database, resources, image_id, holder)?;
    let bytes = std::fs::read(row.path(&LibraryPaths::new(root))?)?;
    drop(guard);
    Ok((row, bytes))
}

pub fn load(database: &LibraryDb, image_id: i64) -> AppResult<ContentRow> {
    if image_id <= 0 {
        return Err(AppError::message("图片 id 无效"));
    }
    database.with_reader(|connection| {
        connection
            .query_row(
                "SELECT images.id, images.content_hash, images.file_ext,
                        folders.path, images.file_size, images.width, images.height,
                        images.format, images.has_alpha, images.phash, images.colors4,
                        images.colors8, images.modified_at, images.thumb_version, images.deleted_at
                 FROM images
                 JOIN folders ON folders.id = images.folder_id
                 WHERE images.id = ?1",
                [image_id],
                |row| {
                    Ok(ContentRow {
                        id: row.get(0)?,
                        content_hash: row.get(1)?,
                        file_ext: row.get(2)?,
                        folder_path: row.get(3)?,
                        file_size: row.get(4)?,
                        width: row.get(5)?,
                        height: row.get(6)?,
                        format: row.get(7)?,
                        has_alpha: row.get::<_, i64>(8)? != 0,
                        phash: row.get(9)?,
                        colors4: row.get(10)?,
                        colors8: row.get(11)?,
                        modified_at: row.get(12)?,
                        thumb_version: row.get(13)?,
                        deleted_at: row.get(14)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| AppError::message("图片不存在"))
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::{
        concurrency::resources::{HolderInfo, ResourceRequest, ResourceTable},
        library::{paths::LibraryPaths, service, service::LibrarySession},
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        session: LibrarySession,
        paths: LibraryPaths,
        resources: ResourceTable,
        image_id: i64,
        bytes: Vec<u8>,
    }

    fn holder(id: &str) -> HolderInfo {
        HolderInfo::new(id, id)
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "内容测试", 1).unwrap();
        let hash = "b".repeat(64);
        let bytes = b"original-bytes".to_vec();
        let paths = LibraryPaths::new(&root);
        let file = metadata::content_path(&paths, "旧", &hash, "png").unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &bytes).unwrap();
        let image_id = session
            .db
            .with_writer(|connection| {
                connection.execute("INSERT INTO folders(path, created_at) VALUES ('旧', 1)", [])?;
                let folder_id = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'png', ?2, ?3, 10, 10, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params![hash, folder_id, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        Fixture {
            _temp: temp,
            session,
            paths,
            resources: ResourceTable::new(),
            image_id,
            bytes,
        }
    }

    #[test]
    fn rereads_the_new_folder_after_a_rename() {
        let fixture = fixture();
        let read = |cancel: &dyn Fn() -> bool| {
            read_original(
                &fixture.session.db,
                &fixture.resources,
                &fixture.paths.root,
                fixture.image_id,
                &holder("reader"),
                cancel,
            )
        };

        let (row, bytes) = read(&|| false).unwrap();
        assert_eq!(row.folder_path, "旧");
        assert_eq!(bytes, fixture.bytes);

        // 文件夹改名后，下一次读取按数据库中的新路径解析，而不是继续用旧快照。
        let new_file =
            metadata::content_path(&fixture.paths, "新", &row.content_hash, "png").unwrap();
        std::fs::create_dir_all(new_file.parent().unwrap()).unwrap();
        std::fs::write(&new_file, &fixture.bytes).unwrap();
        fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE folders SET path = '新'
                     WHERE id = (SELECT folder_id FROM images WHERE id = ?1)",
                    [fixture.image_id],
                )?;
                Ok(())
            })
            .unwrap();
        std::fs::remove_dir_all(fixture.paths.root.join("旧")).unwrap();

        let (row, bytes) = read(&|| false).unwrap();
        assert_eq!(row.folder_path, "新");
        assert_eq!(bytes, fixture.bytes);
    }

    #[test]
    fn a_cancelled_reader_leaves_no_waiter() {
        let fixture = fixture();
        let _writer = fixture
            .resources
            .try_acquire(&ResourceRequest::path_write("旧"), &holder("folder-writer"))
            .unwrap();
        let error = read_original(
            &fixture.session.db,
            &fixture.resources,
            &fixture.paths.root,
            fixture.image_id,
            &holder("reader"),
            &|| true,
        )
        .err()
        .expect("读取应当被取消");
        assert!(error.to_string().contains("读取已取消"));
        assert!(!fixture.resources.has_waiters());
    }
}

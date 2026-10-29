use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::{Mutex, RwLock};
use rusqlite::OptionalExtension;

use crate::{
    app::error::{AppError, AppResult},
    imageops::{colors, phash, thumbnail},
    import::decode,
    library::{content, db::LibraryDb, import as repository, service::now_ms},
};

const HEADER_MAGIC: &[u8; 8] = b"ARTBTHM1";
const HEADER_VERSION: u32 = 1;
pub const HEADER_LEN: u64 = 16;
pub const DEFAULT_SEGMENT_SIZE: u64 = 1024 * 1024 * 1024;
pub const MAX_THUMBNAIL_BYTES: u32 = 32 * 1024 * 1024;
/// 累积这么多新字节就同步一次段文件。
///
/// 每条缩略图都 `sync_all` 会把导入变成逐张 fsync；完全不同步则掉电后数据库里
/// 的指针可能指向段文件末尾之外。取一个字节阈值：最坏丢一个批次的尾部，
/// 由打开图库时的段长度校验把越界指针清掉、下次重建补上。
const SYNC_INTERVAL_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbLocation {
    pub segment: u32,
    pub offset: u64,
    pub length: u32,
}

/// 缩略图指针。`width/height` 供网格按宽高比预留布局，当前协议只返回字节与格式。
#[derive(Debug, Clone)]
pub struct ThumbPointer {
    pub location: ThumbLocation,
    pub format: String,
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
    pub version: u32,
}

pub fn segment_path(directory: &Path, segment: u32) -> PathBuf {
    directory.join(format!("{segment:04}.pack"))
}

/// 追加式缩略图包。写入串行，读取按 segment 缓存句柄并做定位读。
pub struct ThumbPack {
    directory: PathBuf,
    segment_size: u64,
    state: Mutex<PackState>,
    readers: RwLock<HashMap<u32, Arc<File>>>,
}

struct PackState {
    segment: u32,
    offset: u64,
    file: Option<File>,
    /// 自上次 `sync_all` 以来追加的字节数；段轮换与 `sync()` 时清零。
    unsynced: u64,
}

impl ThumbPack {
    pub fn open(directory: impl Into<PathBuf>) -> AppResult<Self> {
        Self::open_with_segment_size(directory, DEFAULT_SEGMENT_SIZE)
    }

    pub fn open_with_segment_size(
        directory: impl Into<PathBuf>,
        segment_size: u64,
    ) -> AppResult<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;

        let mut latest = 0_u32;
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name.strip_suffix(".pack") else {
                continue;
            };
            let Ok(segment) = stem.parse::<u32>() else {
                continue;
            };
            latest = latest.max(segment);
        }

        let offset = if latest == 0 {
            0
        } else {
            read_header(&segment_path(&directory, latest))?
        };

        Ok(Self {
            directory,
            segment_size,
            state: Mutex::new(PackState {
                segment: latest,
                offset,
                file: None,
                unsynced: 0,
            }),
            readers: RwLock::new(HashMap::new()),
        })
    }

    pub fn append(&self, bytes: &[u8]) -> AppResult<ThumbLocation> {
        if bytes.is_empty() {
            return Err(AppError::message("缩略图数据为空"));
        }
        if bytes.len() > MAX_THUMBNAIL_BYTES as usize {
            return Err(AppError::message("缩略图数据超过上限"));
        }

        let mut state = self.state.lock();
        state.ensure_file(&self.directory)?;

        let length = bytes.len() as u64;
        if state.offset > HEADER_LEN && state.offset + length > self.segment_size {
            // 换段前先把旧段落盘：指针已经指向它，丢了就再也读不回来。
            state.sync()?;
            state.segment += 1;
            state.offset = HEADER_LEN;
            state.file = Some(create_segment(&self.directory, state.segment)?);
            state.unsynced = 0;
        }

        let offset = state.offset;
        let file = state.file.as_mut().expect("writer initialized");
        file.write_all(bytes)?;
        file.flush()?;
        state.offset += length;
        state.unsynced += length;
        if state.unsynced >= SYNC_INTERVAL_BYTES {
            state.sync()?;
        }

        Ok(ThumbLocation {
            segment: state.segment,
            offset,
            length: bytes.len() as u32,
        })
    }

    /// 把当前段的写入落到磁盘；没有待同步字节时什么也不做。
    ///
    /// 指针事务提交前调用，让「库里说有缩略图」尽量等于「磁盘上真有这些字节」；
    /// 关闭或切换图库时由析构兜底。
    pub fn sync(&self) -> AppResult<()> {
        self.state.lock().sync()
    }

    pub fn segment_size_bytes(&self, segment: u32) -> AppResult<u64> {
        Ok(std::fs::metadata(segment_path(&self.directory, segment))?.len())
    }

    /// 段文件所在目录；只有测试需要按目录定位段文件。
    #[cfg(test)]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// 包被重写后重新读最新段头，让写指针落到新段末尾。
    ///
    /// 回收任务提交后调用：新段号比原来大，旧段的读句柄已经失效，一起丢掉。
    pub fn reload_from_disk(&self) -> AppResult<()> {
        let mut latest = 0_u32;
        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name.strip_suffix(".pack") else {
                continue;
            };
            if let Ok(segment) = stem.parse::<u32>() {
                latest = latest.max(segment);
            }
        }
        let offset = if latest == 0 {
            0
        } else {
            read_header(&segment_path(&self.directory, latest))?
        };
        {
            let mut state = self.state.lock();
            // 写句柄马上丢弃：先把尾巴落盘，再按新段重设写指针。
            if let Err(error) = state.sync() {
                log::warn!("缩略图段落盘失败：{error}");
            }
            state.file = None;
            state.segment = latest;
            state.offset = offset;
            state.unsynced = 0;
        }
        self.readers.write().clear();
        Ok(())
    }

    pub fn read(&self, location: ThumbLocation) -> AppResult<Vec<u8>> {
        if location.length == 0 || location.length > MAX_THUMBNAIL_BYTES {
            return Err(AppError::message("缩略图指针长度无效"));
        }
        if location.offset < HEADER_LEN {
            return Err(AppError::message("缩略图指针偏移无效"));
        }

        let file = self.reader(location.segment)?;
        let size = file.metadata()?.len();
        let end = location
            .offset
            .checked_add(location.length as u64)
            .ok_or_else(|| AppError::message("缩略图指针越界"))?;
        if end > size {
            return Err(AppError::message("缩略图指针越界"));
        }

        let mut bytes = vec![0_u8; location.length as usize];
        read_exact_at(&file, location.offset, &mut bytes)?;
        Ok(bytes)
    }

    fn reader(&self, segment: u32) -> AppResult<Arc<File>> {
        if let Some(file) = self.readers.read().get(&segment) {
            return Ok(file.clone());
        }
        let file = Arc::new(File::open(segment_path(&self.directory, segment))?);
        self.readers.write().insert(segment, file.clone());
        Ok(file)
    }
}

impl Drop for ThumbPack {
    fn drop(&mut self) {
        if let Err(error) = self.sync() {
            log::warn!("关闭缩略图包时落盘失败：{error}");
        }
    }
}

impl PackState {
    fn ensure_file(&mut self, directory: &Path) -> AppResult<()> {
        if self.file.is_some() {
            return Ok(());
        }
        if self.segment == 0 {
            self.segment = 1;
            self.offset = HEADER_LEN;
            self.file = Some(create_segment(directory, self.segment)?);
        } else {
            let path = segment_path(directory, self.segment);
            let file = OpenOptions::new().read(true).append(true).open(&path)?;
            self.offset = file.metadata()?.len();
            self.file = Some(file);
        }
        self.unsynced = 0;
        Ok(())
    }

    /// 当前段落盘；没有待同步字节时是空操作。
    fn sync(&mut self) -> AppResult<()> {
        if self.unsynced == 0 {
            return Ok(());
        }
        let file = self.file.as_mut().expect("有未落盘字节时写句柄必然存在");
        file.flush()?;
        file.sync_all()?;
        self.unsynced = 0;
        Ok(())
    }
}

fn create_segment(directory: &Path, segment: u32) -> AppResult<File> {
    let path = segment_path(directory, segment);
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(&path)?;
    let mut header = [0_u8; HEADER_LEN as usize];
    header[..8].copy_from_slice(HEADER_MAGIC);
    header[8..12].copy_from_slice(&HEADER_VERSION.to_le_bytes());
    file.write_all(&header)?;
    file.flush()?;
    Ok(file)
}

fn read_header(path: &Path) -> AppResult<u64> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if size < HEADER_LEN {
        return Err(AppError::message(format!(
            "缩略图包 {} 头部不完整",
            path.display()
        )));
    }
    let mut header = [0_u8; HEADER_LEN as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != HEADER_MAGIC {
        return Err(AppError::message(format!(
            "缩略图包 {} 格式不匹配",
            path.display()
        )));
    }
    let version = u32::from_le_bytes(header[8..12].try_into().expect("header version"));
    if version != HEADER_VERSION {
        return Err(AppError::message(format!(
            "缩略图包 {} 版本 {version} 不受支持",
            path.display()
        )));
    }
    Ok(size)
}

/// 段长度校验的结果；只在真的修过东西时非零。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ThumbRepair {
    /// 整段文件不见了。
    pub missing_segments: u32,
    /// 段文件比 `thumb_segments.size` 记录的短（掉电后尾部丢失）。
    pub truncated_segments: u32,
    /// 被清掉指针的图片行数。
    pub cleared_pointers: u64,
}

/// 打开图库时校验段文件长度，把指向段尾之外的缩略图指针清掉。
///
/// 指针事务是持久的，而段文件在掉电时可能丢掉最后一截；不清掉这些指针，
/// 网格会一直 404 且没有任何东西会去修它。清空后这些行重新算“缺缩略图”，
/// 下一次重建缩略图任务就会补上。
pub fn validate_segments(database: &LibraryDb, directory: &Path) -> AppResult<ThumbRepair> {
    let segments: Vec<(u32, i64)> = database.with_reader(|connection| {
        let mut statement =
            connection.prepare("SELECT id, size FROM thumb_segments WHERE id > 0")?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;

    let mut report = ThumbRepair::default();
    let mut repairs: Vec<(u32, u64)> = Vec::new();
    for (segment, recorded) in segments {
        let recorded = u64::try_from(recorded).unwrap_or(0);
        match std::fs::metadata(segment_path(directory, segment)) {
            Ok(metadata) => {
                let actual = metadata.len();
                if actual < recorded {
                    log::warn!(
                        "缩略图段 {segment:04} 只有 {actual} 字节，记录是 {recorded} 字节；清掉越界指针"
                    );
                    report.truncated_segments += 1;
                    repairs.push((segment, actual));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                log::warn!("缩略图段 {segment:04} 的段文件不见了；清空这一段的指针");
                report.missing_segments += 1;
                repairs.push((segment, 0));
            }
            Err(error) => log::warn!("读取缩略图段 {segment:04} 失败：{error}"),
        }
    }
    if repairs.is_empty() {
        return Ok(report);
    }

    database.with_writer(|connection| {
        let transaction = connection.transaction()?;
        for (segment, usable) in &repairs {
            report.cleared_pointers += transaction.execute(
                "UPDATE images
                 SET thumb_segment = 0, thumb_offset = 0, thumb_length = 0
                 WHERE thumb_segment = ?1 AND thumb_offset + thumb_length > ?2",
                rusqlite::params![segment, i64::try_from(*usable).unwrap_or(i64::MAX)],
            )? as u64;
        }
        transaction.commit()?;
        Ok(())
    })?;
    Ok(report)
}

pub fn load_pointer(database: &LibraryDb, image_id: i64) -> AppResult<Option<ThumbPointer>> {
    database.with_reader(|connection| {
        let pointer = connection
            .query_row(
                "SELECT thumb_segment, thumb_offset, thumb_length, thumb_format,
                        thumb_width, thumb_height, thumb_version
                 FROM images WHERE id = ?1",
                [image_id],
                |row| {
                    Ok(ThumbPointer {
                        location: ThumbLocation {
                            segment: row.get(0)?,
                            offset: row.get::<_, i64>(1)? as u64,
                            length: row.get(2)?,
                        },
                        format: row.get(3)?,
                        width: row.get(4)?,
                        height: row.get(5)?,
                        version: row.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(pointer.filter(|pointer| pointer.location.segment != 0 && pointer.location.length > 0))
    })
}

/// 重新编码单张缩略图并更新指针；可选重算 pHash 与主色。
///
/// 返回 `false` 表示跳过（回收站图片，或已有缩略图且只补缺失项）。
/// 追加写入，旧 blob 留给后续整理，不在这里回收。
#[allow(clippy::too_many_arguments)]
pub fn rebuild(
    database: &LibraryDb,
    pack: &ThumbPack,
    root: &Path,
    image_id: i64,
    only_missing: bool,
    recalc_visuals: bool,
    resources: &crate::concurrency::resources::ResourceTable,
    holder: &crate::concurrency::resources::HolderInfo,
    cancel: &dyn Fn() -> bool,
    images: &crate::concurrency::budget::ImageBudget,
) -> AppResult<bool> {
    let row = content::load(database, image_id)?;
    if row.deleted_at.is_some() {
        return Ok(false);
    }
    if only_missing && load_pointer(database, image_id)?.is_some() {
        return Ok(false);
    }

    // 先读原图（路径锁在返回前已释放），再拿图像预算做解码与编码。
    let (row, bytes) = content::read_original(database, resources, root, image_id, holder, cancel)?;
    let _permit = images
        .acquire(cancel)
        .map_err(|_| AppError::message("图像预算等待已取消"))?;
    let decoded = decode::decode_bytes(&bytes, Some(&row.file_ext))?;
    let thumb = thumbnail::encode(&decoded.image)?;
    let visuals = recalc_visuals.then(|| {
        (
            phash::compute(&decoded.image),
            colors::dominant(&decoded.image),
        )
    });
    let location = pack.append(&thumb.bytes)?;
    let offset = i64::try_from(location.offset)
        .map_err(|_| AppError::message("缩略图偏移超出数据库范围"))?;
    let now = now_ms();

    database.with_writer(|connection| {
        let transaction = connection.transaction()?;
        let changed = match &visuals {
            Some((hash, (colors4, colors8))) => transaction.execute(
                "UPDATE images SET
                    thumb_segment = ?1, thumb_offset = ?2, thumb_length = ?3,
                    thumb_format = 'webp', thumb_width = ?4, thumb_height = ?5,
                    thumb_version = thumb_version + 1,
                    phash = ?6, colors4 = ?7, colors8 = ?8
                 WHERE id = ?9",
                rusqlite::params![
                    location.segment,
                    offset,
                    location.length,
                    thumb.width,
                    thumb.height,
                    hash.as_slice(),
                    colors4.as_slice(),
                    colors8.as_slice(),
                    image_id
                ],
            )?,
            None => transaction.execute(
                "UPDATE images SET
                    thumb_segment = ?1, thumb_offset = ?2, thumb_length = ?3,
                    thumb_format = 'webp', thumb_width = ?4, thumb_height = ?5,
                    thumb_version = thumb_version + 1
                 WHERE id = ?6",
                rusqlite::params![
                    location.segment,
                    offset,
                    location.length,
                    thumb.width,
                    thumb.height,
                    image_id
                ],
            )?,
        };
        if changed == 0 {
            return Err(AppError::message("图片不存在或已被删除"));
        }
        repository::ensure_thumb_segment(&transaction, location.segment, now)?;
        if let Ok(size) = pack.segment_size_bytes(location.segment) {
            repository::update_thumb_segment_size(&transaction, location.segment, size)?;
        }
        // 指针提交前落盘：重建是后台任务，多一次 fsync 换来掉电后不用整库重来。
        if let Err(error) = pack.sync() {
            log::warn!("缩略图段落盘失败：{error}");
        }
        transaction.commit()?;
        Ok(())
    })?;
    Ok(true)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;

    while !buffer.is_empty() {
        match file.seek_read(buffer, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "缩略图包提前结束",
                ));
            }
            Ok(read) => {
                offset += read as u64;
                buffer = &mut buffer[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    while !buffer.is_empty() {
        match file.read_at(buffer, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "缩略图包提前结束",
                ));
            }
            Ok(read) => {
                offset += read as u64;
                buffer = &mut buffer[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(any(windows, unix)))]
fn read_exact_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};

    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_blobs_and_reopens() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("thumbs");

        let locations = {
            let pack = ThumbPack::open(&directory).unwrap();
            let first = pack.append(b"first-thumbnail").unwrap();
            let second = pack.append(b"second-thumbnail").unwrap();
            assert_eq!(first.segment, 1);
            assert_eq!(first.offset, HEADER_LEN);
            assert_eq!(first.length, 15);
            assert_eq!(second.offset, HEADER_LEN + 15);
            pack.read(first).unwrap();
            vec![first, second]
        };

        let reopened = ThumbPack::open(&directory).unwrap();
        assert_eq!(reopened.read(locations[0]).unwrap(), b"first-thumbnail");
        assert_eq!(reopened.read(locations[1]).unwrap(), b"second-thumbnail");

        let third = reopened.append(b"third-thumbnail").unwrap();
        assert_eq!(third.segment, 1);
        assert_eq!(third.offset, HEADER_LEN + 31);
        assert_eq!(reopened.read(locations[1]).unwrap(), b"second-thumbnail");
    }

    #[test]
    fn rotates_segments() {
        let temp = tempfile::tempdir().unwrap();
        let pack = ThumbPack::open_with_segment_size(temp.path(), HEADER_LEN + 10).unwrap();

        let first = pack.append(&[1_u8; 6]).unwrap();
        let second = pack.append(&[2_u8; 6]).unwrap();
        let third = pack.append(&[3_u8; 6]).unwrap();

        assert_eq!(first.segment, 1);
        assert_eq!(second.segment, 2);
        assert_eq!(second.offset, HEADER_LEN);
        assert_eq!(third.segment, 3);
        assert_eq!(third.offset, HEADER_LEN);
        assert_eq!(pack.read(first).unwrap(), vec![1_u8; 6]);
        assert_eq!(pack.read(second).unwrap(), vec![2_u8; 6]);
    }

    #[test]
    fn rejects_invalid_pointers() {
        let temp = tempfile::tempdir().unwrap();
        let pack = ThumbPack::open(temp.path()).unwrap();
        let location = pack.append(b"abc").unwrap();

        assert!(
            pack.read(ThumbLocation {
                length: 0,
                ..location
            })
            .is_err()
        );
        assert!(
            pack.read(ThumbLocation {
                length: 999,
                ..location
            })
            .is_err()
        );
        assert!(
            pack.read(ThumbLocation {
                segment: 9,
                ..location
            })
            .is_err()
        );
    }

    /// 包被重写后重新读段头：新的 append 必须落到最新段末尾，不能写回旧段。
    #[test]
    fn reload_points_writes_at_the_latest_segment() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("thumbs");
        let pack = ThumbPack::open(&directory).unwrap();
        pack.append(b"first").unwrap();

        // 造一个回收后的 2 号段（段头必须是真的，否则重读会拒绝）。
        let mut header = [0_u8; HEADER_LEN as usize];
        header[..8].copy_from_slice(HEADER_MAGIC);
        header[8..12].copy_from_slice(&HEADER_VERSION.to_le_bytes());
        let mut file = File::create(segment_path(&directory, 2)).unwrap();
        file.write_all(&header).unwrap();
        file.write_all(b"moved").unwrap();
        drop(file);

        pack.reload_from_disk().unwrap();
        let location = pack.append(b"second").unwrap();
        assert_eq!(location.segment, 2);
        assert_eq!(location.offset, HEADER_LEN + 5);
        assert_eq!(pack.read(location).unwrap(), b"second");
    }

    #[test]
    fn loads_pointer_from_database() {
        use rusqlite::params;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _manifest) = crate::library::service::create(&root, "测试", 1).unwrap();
        let location = session.thumbs.append(b"thumbnail-bytes").unwrap();

        session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at, thumb_segment, thumb_offset, thumb_length, thumb_format,
                        thumb_width, thumb_height, thumb_version
                     ) VALUES (?1, 'webp', 1, 100, 512, 512, 'webp', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1,
                        ?2, ?3, ?4, 'webp', 512, 512, 3)",
                    params![
                        "a".repeat(64),
                        location.segment,
                        location.offset,
                        location.length
                    ],
                )?;
                Ok(())
            })
            .unwrap();

        let pointer = load_pointer(&session.db, 1).unwrap().unwrap();
        assert_eq!(pointer.location, location);
        assert_eq!(pointer.format, "webp");
        assert_eq!(pointer.version, 3);
        assert_eq!(
            session.thumbs.read(pointer.location).unwrap(),
            b"thumbnail-bytes"
        );
        assert!(load_pointer(&session.db, 2).unwrap().is_none());
    }

    #[test]
    fn clears_pointers_past_a_truncated_segment() {
        use rusqlite::params;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = crate::library::service::create(&root, "段校验", 1).unwrap();
        let first = session.thumbs.append(b"first-thumbnail").unwrap();
        let second = session.thumbs.append(b"second-thumbnail").unwrap();
        assert_eq!(first.segment, second.segment);
        session.thumbs.sync().unwrap();

        let ids = session
            .db
            .with_writer(|connection| {
                let mut ids = Vec::new();
                for (index, location) in [first, second].iter().enumerate() {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at, thumb_segment, thumb_offset, thumb_length, thumb_format,
                            thumb_width, thumb_height, thumb_version
                         ) VALUES (?1, 'webp', 1, 100, 512, 512, 'webp', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1,
                            ?2, ?3, ?4, 'webp', 512, 512, 1)",
                        params![
                            format!("{index:0>64}"),
                            location.segment,
                            location.offset,
                            location.length
                        ],
                    )?;
                    ids.push(connection.last_insert_rowid());
                }
                crate::library::import::ensure_thumb_segment(connection, first.segment, 1)?;
                crate::library::import::update_thumb_segment_size(
                    connection,
                    first.segment,
                    HEADER_LEN + 30,
                )?;
                Ok(ids)
            })
            .unwrap();

        // 模拟掉电：段文件只剩第一条缩略图。
        let path = segment_path(session.thumbs.directory(), first.segment);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(HEADER_LEN + u64::from(first.length)).unwrap();
        drop(file);

        let report = validate_segments(&session.db, session.thumbs.directory()).unwrap();
        assert_eq!(report.truncated_segments, 1);
        assert_eq!(report.cleared_pointers, 1);
        assert!(load_pointer(&session.db, ids[0]).unwrap().is_some());
        assert!(
            load_pointer(&session.db, ids[1]).unwrap().is_none(),
            "越界指针必须被清掉，否则网格会一直 404"
        );
    }

    #[test]
    fn rebuilds_thumbnail_and_updates_pointer() {
        use rusqlite::params;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _manifest) = crate::library::service::create(&root, "重建测试", 1).unwrap();

        let image = image::RgbImage::from_fn(120, 80, |x, y| {
            image::Rgb([(x * 2) as u8, (y * 3) as u8, 90])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        std::fs::write(root.join(format!("{hash}.png")), &bytes).unwrap();

        let image_id = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'png', 1, ?2, 120, 80, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params![hash, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();

        assert!(
            rebuild(
                &session.db,
                &session.thumbs,
                &root,
                image_id,
                true,
                true,
                &crate::concurrency::resources::ResourceTable::new(),
                &crate::concurrency::resources::HolderInfo::new("test", "test"),
                &|| false,
                &crate::concurrency::budget::ImageBudget::new(2)
            )
            .unwrap()
        );
        let pointer = load_pointer(&session.db, image_id).unwrap().unwrap();
        assert_eq!(pointer.format, "webp");
        assert_eq!((pointer.width, pointer.height), (120, 80));
        assert_eq!(pointer.version, 2);
        let decoded =
            image::load_from_memory(&session.thumbs.read(pointer.location).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (120, 80));

        // 只补缺失项时，已有缩略图的图片直接跳过。
        assert!(
            !rebuild(
                &session.db,
                &session.thumbs,
                &root,
                image_id,
                true,
                false,
                &crate::concurrency::resources::ResourceTable::new(),
                &crate::concurrency::resources::HolderInfo::new("test", "test"),
                &|| false,
                &crate::concurrency::budget::ImageBudget::new(2)
            )
            .unwrap()
        );
        assert!(
            rebuild(
                &session.db,
                &session.thumbs,
                &root,
                image_id,
                false,
                false,
                &crate::concurrency::resources::ResourceTable::new(),
                &crate::concurrency::resources::HolderInfo::new("test", "test"),
                &|| false,
                &crate::concurrency::budget::ImageBudget::new(2)
            )
            .unwrap()
        );
        assert_eq!(
            load_pointer(&session.db, image_id)
                .unwrap()
                .unwrap()
                .version,
            3
        );
    }
}

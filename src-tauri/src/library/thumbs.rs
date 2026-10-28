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
            if let Some(file) = state.file.as_mut() {
                file.flush()?;
            }
            state.segment += 1;
            state.offset = HEADER_LEN;
            state.file = Some(create_segment(&self.directory, state.segment)?);
        }

        let offset = state.offset;
        let file = state.file.as_mut().expect("writer initialized");
        file.write_all(bytes)?;
        file.flush()?;
        state.offset += length;

        Ok(ThumbLocation {
            segment: state.segment,
            offset,
            length: bytes.len() as u32,
        })
    }

    pub fn segment_size_bytes(&self, segment: u32) -> AppResult<u64> {
        Ok(std::fs::metadata(segment_path(&self.directory, segment))?.len())
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

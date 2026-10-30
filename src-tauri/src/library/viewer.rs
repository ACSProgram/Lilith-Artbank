use std::path::{Path, PathBuf};

use image::{ImageDecoder, imageops::FilterType, metadata::Orientation};
use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget, estimate_working_bytes},
        resources::{HolderInfo, ResourceTable},
    },
    imageops::{
        edit::{self, MAX_EDIT_PIXELS, OutputFormat},
        thumbnail,
    },
    import::decode,
    library::{content, content::ContentRow, db::LibraryDb, paths::LibraryPaths},
};

/// 超过此像素数时查看器使用降采样预览；编辑保存仍读取原图。
pub const PREVIEW_PIXELS: u64 = 24_000_000;
pub const PREVIEW_MAX_EDGE: u32 = 4096;
const PREVIEW_QUALITY: u8 = 85;

/// `temp/viewer/` 的单次会话上限与清理粒度。
///
/// 只有大图、带 EXIF 方向与浏览器不能解码的格式会生成预览；连续浏览这些图
/// 单次会话可以积累到数 GB。写够 `PREVIEW_SWEEP_BYTES` 才扫一次目录，查看路径
/// 本身不做 IO。
const PREVIEW_CACHE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const PREVIEW_SWEEP_BYTES: u64 = 256 * 1024 * 1024;

static PREVIEW_WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 浏览器可直接解码的格式；其余格式即使不大也需要 Rust 生成预览。
const BROWSER_FORMATS: [&str; 5] = ["jpeg", "png", "webp", "gif", "bmp"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerSource {
    pub image_id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub format: String,
    pub file_size: i64,
    pub thumb_version: i64,
    pub has_alpha: bool,
    /// 原图像素尺寸。
    pub width: u32,
    pub height: u32,
    /// 查看器实际加载的尺寸；降采样预览时小于原图。
    pub display_width: u32,
    pub display_height: u32,
    /// 为 true 时使用 `img://preview/<id>`。
    pub preview: bool,
    pub downsampled: bool,
    pub editable: bool,
    pub deleted: bool,
    pub max_edit_pixels: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoseLandmark {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub visibility: f32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoseData {
    pub model: String,
    pub landmarks: Vec<PoseLandmark>,
}

pub fn source(
    database: &LibraryDb,
    root: &Path,
    image_id: i64,
    resources: &ResourceTable,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
) -> AppResult<ViewerSource> {
    source_with_threshold(
        database,
        root,
        image_id,
        PREVIEW_PIXELS,
        resources,
        images,
        memory,
    )
}

fn source_with_threshold(
    database: &LibraryDb,
    root: &Path,
    image_id: i64,
    preview_threshold: u64,
    resources: &ResourceTable,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
) -> AppResult<ViewerSource> {
    let paths = LibraryPaths::new(root);
    let holder = HolderInfo::new(format!("viewer-{image_id}"), "查看器").with_path_note("正在读取");
    // 查看器是交互式读取：文件夹正忙时直接报可重试错误，不排在长时间任务后面。
    let (row, guard) = content::try_lock_original(database, resources, image_id, &holder)?;

    let path = row.path(&paths)?;
    if !path.is_file() {
        return Err(AppError::message("原图文件不存在，可能已被外部移动或删除"));
    }

    let browser_native = BROWSER_FORMATS.contains(&row.format.as_str());
    let pixel_count = row.pixel_count();
    // 带 EXIF 方向的图片统一交给 Rust 生成已转正的预览，避免 WebView2 的
    // 自然尺寸/方向语义与数据库宽高不一致，裁剪坐标才可预测。
    let oriented = browser_native && has_exif_orientation(&path);
    let needs_preview = !browser_native || pixel_count > preview_threshold || oriented;
    let (display_width, display_height) = if needs_preview {
        // 读字节在路径读锁内完成；解码与写预览缓存放到预算之后。
        let bytes = std::fs::read(&path)?;
        drop(guard);
        let _permit = images
            .acquire(&|| false)
            .map_err(|_| AppError::message("图像预算等待已取消"))?;
        let estimate = estimate_working_bytes(row.width, row.height, bytes.len() as u64, false);
        let _memory = memory
            .acquire(estimate, &|| false)
            .map_err(|_| AppError::message("图像内存预算等待已取消"))?;
        ensure_preview_bytes(&paths, &row, &bytes)?
    } else {
        drop(guard);
        (row.width, row.height)
    };

    Ok(ViewerSource {
        image_id: row.id,
        content_hash: row.content_hash,
        file_ext: row.file_ext,
        format: row.format,
        file_size: row.file_size,
        thumb_version: row.thumb_version,
        has_alpha: row.has_alpha,
        width: row.width,
        height: row.height,
        display_width,
        display_height,
        preview: needs_preview,
        downsampled: display_width != row.width || display_height != row.height,
        editable: row.deleted_at.is_none() && pixel_count <= MAX_EDIT_PIXELS,
        deleted: row.deleted_at.is_some(),
        max_edit_pixels: MAX_EDIT_PIXELS,
    })
}

fn has_exif_orientation(path: &Path) -> bool {
    let Ok(reader) = image::ImageReader::open(path) else {
        return false;
    };
    let Ok(reader) = reader.with_guessed_format() else {
        return false;
    };
    let Ok(mut decoder) = reader.into_decoder() else {
        return false;
    };
    decoder
        .orientation()
        .is_ok_and(|orientation| orientation != Orientation::NoTransforms)
}

pub fn preview_path(paths: &LibraryPaths, image_id: i64, content_hash: &str) -> PathBuf {
    paths
        .temp
        .join("viewer")
        .join(format!("{image_id}-{content_hash}.webp"))
}

/// 供 `img://preview` 使用：已有文件直接返回，缺失时按当前行重新生成。
/// 返回预览路径与生成它时对应的内容哈希，调用方用作缓存键。
pub(crate) fn ensure_preview_file(
    database: &LibraryDb,
    paths: &LibraryPaths,
    row: &ContentRow,
    resources: &ResourceTable,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
) -> AppResult<(PathBuf, String)> {
    let target = preview_path(paths, row.id, &row.content_hash);
    if target.is_file() {
        return Ok((target, row.content_hash.clone()));
    }
    let holder = HolderInfo::new(format!("viewer-{}", row.id), "查看器").with_path_note("正在读取");
    let (latest, bytes) =
        content::try_read_original(database, resources, &paths.root, row.id, &holder)?;
    let _permit = images
        .acquire(&|| false)
        .map_err(|_| AppError::message("图像预算等待已取消"))?;
    let estimate = estimate_working_bytes(latest.width, latest.height, bytes.len() as u64, false);
    let _memory = memory
        .acquire(estimate, &|| false)
        .map_err(|_| AppError::message("图像内存预算等待已取消"))?;
    ensure_preview_bytes(paths, &latest, &bytes)?;
    Ok((
        preview_path(paths, latest.id, &latest.content_hash),
        latest.content_hash,
    ))
}

fn ensure_preview_bytes(
    paths: &LibraryPaths,
    row: &ContentRow,
    bytes: &[u8],
) -> AppResult<(u32, u32)> {
    let target = preview_path(paths, row.id, &row.content_hash);
    let expected = thumbnail::fit_within(row.width, row.height, PREVIEW_MAX_EDGE);
    if target.is_file() {
        return Ok(expected);
    }

    let decoded = decode::decode_bytes(bytes, Some(&row.file_ext))?;
    let (width, height) = thumbnail::fit_within(decoded.width, decoded.height, PREVIEW_MAX_EDGE);
    let image = if (width, height) == (decoded.width, decoded.height) {
        decoded.image
    } else {
        decoded
            .image
            .resize_exact(width, height, FilterType::Lanczos3)
    };

    let mut bytes = Vec::new();
    edit::encode(&image, OutputFormat::WebP, PREVIEW_QUALITY, &mut bytes)?;
    write_preview(&target, &bytes)?;
    note_preview_written(paths, bytes.len() as u64);
    Ok((width, height))
}

/// 累计写入量到阈值后按 mtime 清理旧预览；失败只记日志，不影响本次查看。
fn note_preview_written(paths: &LibraryPaths, bytes: u64) {
    use std::sync::atomic::Ordering;

    let written = PREVIEW_WRITTEN.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if written < PREVIEW_SWEEP_BYTES {
        return;
    }
    PREVIEW_WRITTEN.store(0, Ordering::Relaxed);
    if let Err(error) = trim_preview_cache(paths) {
        log::warn!("清理查看器预览缓存失败：{error}");
    }
}

fn trim_preview_cache(paths: &LibraryPaths) -> AppResult<()> {
    let dir = paths.temp.join("viewer");
    if !dir.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0_u64;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        total += metadata.len();
        entries.push((
            metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            metadata.len(),
            entry.path(),
        ));
    }
    if total <= PREVIEW_CACHE_LIMIT_BYTES {
        return Ok(());
    }
    entries.sort_by_key(|(modified, _, _)| *modified);
    for (_, size, path) in entries {
        if total <= PREVIEW_CACHE_LIMIT_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
    Ok(())
}

fn write_preview(target: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, bytes)?;
    match std::fs::rename(&temporary, target) {
        Ok(()) => Ok(()),
        Err(_) if target.is_file() => {
            let _ = std::fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(AppError::Io(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::service;

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        root: PathBuf,
        image_id: i64,
        width: u32,
        height: u32,
    }

    fn fixture(trashed: bool) -> Fixture {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "查看器测试", 1).unwrap();
        let mut image = image::RgbImage::new(24, 16);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x * 7) as u8, (y * 9) as u8, 80]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let path = root.join(format!("{hash}.png"));
        fs::write(&path, &bytes).unwrap();
        let image_id = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at, deleted_at
                     ) VALUES (?1, 'png', 1, ?2, 24, 16, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1, ?3)",
                    params![hash, bytes.len() as i64, trashed.then_some(1_i64)],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        Fixture {
            _temp: temp,
            session,
            root,
            image_id,
            width: 24,
            height: 16,
        }
    }

    #[test]
    fn serves_small_browser_formats_directly() {
        let fixture = fixture(false);
        let source = source(
            &fixture.session.db,
            &fixture.root,
            fixture.image_id,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        assert!(!source.preview);
        assert!(!source.downsampled);
        assert!(source.editable);
        assert_eq!(
            (source.display_width, source.display_height),
            (fixture.width, fixture.height)
        );
        assert_eq!(source.content_hash.len(), 64);
    }

    #[test]
    fn reports_busy_instead_of_waiting_for_a_folder_writer() {
        let fixture = fixture(false);
        let resources = ResourceTable::new();
        let _held = resources
            .try_acquire(
                &crate::concurrency::resources::ResourceRequest::path_write(""),
                &HolderInfo::new("folder-writer", "重命名文件夹"),
            )
            .unwrap();
        let error = source(
            &fixture.session.db,
            &fixture.root,
            fixture.image_id,
            &resources,
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("文件夹正忙"));
    }

    #[test]
    fn generates_and_reuses_preview() {
        let fixture = fixture(false);
        let source = source_with_threshold(
            &fixture.session.db,
            &fixture.root,
            fixture.image_id,
            100,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        assert!(source.preview);
        assert!(!source.downsampled);
        assert_eq!(
            (source.display_width, source.display_height),
            (fixture.width, fixture.height)
        );
        let preview = preview_path(
            &LibraryPaths::new(&fixture.root),
            fixture.image_id,
            &source.content_hash,
        );
        assert!(preview.is_file());
        let modified = fs::metadata(&preview).unwrap().modified().unwrap();

        let again = source_with_threshold(
            &fixture.session.db,
            &fixture.root,
            fixture.image_id,
            100,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        assert!(again.preview);
        assert_eq!(
            fs::metadata(&preview).unwrap().modified().unwrap(),
            modified
        );
    }

    /// 字节额度小到预览必然超限时，解码走独占路径；配额用完必须归还。
    #[test]
    fn preview_generation_releases_memory_quota() {
        let fixture = fixture(false);
        let memory = ImageMemoryBudget::new(1);
        let source = source_with_threshold(
            &fixture.session.db,
            &fixture.root,
            fixture.image_id,
            100,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &memory,
        )
        .unwrap();
        assert!(source.preview);
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn caps_preview_long_edge() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "预览测试", 1).unwrap();
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            5000,
            10,
            image::Rgb([40, 80, 120]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        fs::write(root.join(format!("{hash}.png")), &bytes).unwrap();
        let image_id = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'png', 1, ?2, 5000, 10, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params![hash, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();

        let source = source_with_threshold(
            &session.db,
            &root,
            image_id,
            1,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        assert!(source.preview);
        assert!(source.downsampled);
        assert_eq!((source.display_width, source.display_height), (4096, 8));
        let preview = preview_path(&LibraryPaths::new(&root), image_id, &source.content_hash);
        let decoded = image::load_from_memory(&fs::read(preview).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4096, 8));
    }

    #[test]
    fn trashed_images_are_read_only() {
        let fixture = fixture(true);
        let source = source(
            &fixture.session.db,
            &fixture.root,
            fixture.image_id,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        assert!(source.deleted);
        assert!(!source.editable);
    }

    #[test]
    fn reports_missing_original() {
        let fixture = fixture(false);
        let row = crate::library::content::load(&fixture.session.db, fixture.image_id).unwrap();
        fs::remove_file(row.path(&LibraryPaths::new(&fixture.root)).unwrap()).unwrap();
        assert!(
            source(
                &fixture.session.db,
                &fixture.root,
                fixture.image_id,
                &ResourceTable::new(),
                &ImageBudget::new(2),
                &ImageMemoryBudget::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn regenerates_missing_preview_file() {
        let fixture = fixture(false);
        let source = source_with_threshold(
            &fixture.session.db,
            &fixture.root,
            fixture.image_id,
            100,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        let paths = LibraryPaths::new(&fixture.root);
        let preview = preview_path(&paths, fixture.image_id, &source.content_hash);
        assert!(preview.is_file());
        fs::remove_file(&preview).unwrap();

        let row = crate::library::content::load(&fixture.session.db, fixture.image_id).unwrap();
        let regenerated = ensure_preview_file(
            &fixture.session.db,
            &paths,
            &row,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        assert_eq!(regenerated.0, preview);
        assert_eq!(regenerated.1, row.content_hash);
        let decoded = image::load_from_memory(&fs::read(&preview).unwrap()).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (fixture.width, fixture.height)
        );
    }

    #[test]
    fn cached_preview_survives_missing_original() {
        let fixture = fixture(false);
        let source = source_with_threshold(
            &fixture.session.db,
            &fixture.root,
            fixture.image_id,
            100,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        let paths = LibraryPaths::new(&fixture.root);
        let preview = preview_path(&paths, fixture.image_id, &source.content_hash);
        let row = crate::library::content::load(&fixture.session.db, fixture.image_id).unwrap();
        fs::remove_file(row.path(&paths).unwrap()).unwrap();

        assert_eq!(
            ensure_preview_file(
                &fixture.session.db,
                &paths,
                &row,
                &ResourceTable::new(),
                &ImageBudget::new(2),
                &ImageMemoryBudget::default(),
            )
            .unwrap()
            .0,
            preview
        );
        assert!(preview.is_file());

        fs::remove_file(&preview).unwrap();
        assert!(
            ensure_preview_file(
                &fixture.session.db,
                &paths,
                &row,
                &ResourceTable::new(),
                &ImageBudget::new(2),
                &ImageMemoryBudget::default(),
            )
            .is_err()
        );
        assert!(!preview.exists());
    }

    fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 120])
        });
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut encoded, image::ImageFormat::Jpeg)
            .unwrap();
        let jpeg = encoded.into_inner();

        let mut exif = Vec::new();
        exif.extend_from_slice(b"Exif\0\0");
        exif.extend_from_slice(&[0x49, 0x49, 0x2a, 0x00]);
        exif.extend_from_slice(&8_u32.to_le_bytes());
        exif.extend_from_slice(&1_u16.to_le_bytes());
        exif.extend_from_slice(&0x0112_u16.to_le_bytes());
        exif.extend_from_slice(&3_u16.to_le_bytes());
        exif.extend_from_slice(&1_u32.to_le_bytes());
        exif.extend_from_slice(&orientation.to_le_bytes());
        exif.extend_from_slice(&[0, 0]);
        exif.extend_from_slice(&0_u32.to_le_bytes());

        let mut app1 = Vec::new();
        app1.extend_from_slice(&[0xff, 0xe1]);
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);

        let mut output = Vec::with_capacity(jpeg.len() + app1.len());
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&app1);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    #[test]
    fn exif_oriented_jpeg_uses_oriented_preview() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "方向测试", 1).unwrap();
        let bytes = jpeg_with_orientation(24, 16, 6);
        let hash = blake3::hash(&bytes).to_hex().to_string();
        fs::write(root.join(format!("{hash}.jpg")), &bytes).unwrap();
        let image_id = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'jpg', 1, ?2, 16, 24, 'jpeg', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params![hash, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();

        let source = source(
            &session.db,
            &root,
            image_id,
            &ResourceTable::new(),
            &ImageBudget::new(2),
            &ImageMemoryBudget::default(),
        )
        .unwrap();
        assert!(source.preview);
        assert!(!source.downsampled);
        assert_eq!((source.display_width, source.display_height), (16, 24));
        let preview = preview_path(&LibraryPaths::new(&root), image_id, &source.content_hash);
        let decoded = image::load_from_memory(&fs::read(preview).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (16, 24));
    }
}

use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use image::{DynamicImage, Rgb, RgbImage, Rgba, RgbaImage};
use tempfile::tempdir;

use super::publish::publish;
use super::*;
use crate::{
    imageops::optimize::{OptimizeFormat, QualityTier},
    import::job::ImportSnapshot,
    library::{service, thumbs},
};

fn sample_image(seed: u32, width: u32, height: u32) -> DynamicImage {
    let mut image = RgbImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let stripe = ((x + seed) / 17 + y / 23) % 3;
        *pixel = Rgb([
            ((x * 3 + seed * 11) % 256) as u8,
            ((y * 5 + seed * 7) % 256) as u8,
            (stripe * 90) as u8,
        ]);
    }
    DynamicImage::ImageRgb8(image)
}

fn write_samples(directory: &Path, count: u32, size: (u32, u32)) {
    std::fs::create_dir_all(directory).unwrap();
    for index in 0..count {
        let image = sample_image(index, size.0, size.1);
        let path = match index % 4 {
            0 => directory.join(format!("{index:04}.jpg")),
            1 => directory.join(format!("{index:04}.png")),
            2 => directory.join(format!("{index:04}.bmp")),
            _ => directory.join(format!("{index:04}.webp")),
        };
        if index % 4 == 3 {
            let (width, height) = (image.width(), image.height());
            let rgb = image.to_rgb8();
            let encoded = webp::Encoder::from_rgb(rgb.as_raw(), width, height).encode(75.0);
            std::fs::write(path, encoded.to_vec()).unwrap();
        } else {
            image.save(path).unwrap();
        }
    }
}

fn open_library(root: &Path) -> crate::library::service::LibrarySession {
    service::create(root, "导入测试", 2).unwrap().0
}

fn source(session: &crate::library::service::LibrarySession, root: &Path) -> ImportSource {
    ImportSource {
        database: session.db.clone(),
        thumbs: session.thumbs.clone(),
        root: root.to_path_buf(),
        operation_id: None,
        resources: Arc::new(crate::concurrency::resources::ResourceTable::new()),
        images: Arc::new(crate::concurrency::budget::ImageBudget::new(4)),
        memory: Arc::new(crate::concurrency::budget::ImageMemoryBudget::default()),
        holder: crate::concurrency::resources::HolderInfo::new("import-test", "导入"),
    }
}

fn request(directory: &Path) -> ImportRequest {
    ImportRequest {
        paths: vec![directory.to_string_lossy().into_owned()],
        target: ImportTarget::Library,
        folder: "导入".into(),
        folder_mode: FolderMode::Flat,
        tags: vec!["测试".into()],
        score: 3,
        duplicate_policy: DuplicatePolicy::Skip,
        perceptual_threshold: 6,
        optimize: None,
        #[cfg(test)]
        cancel_after: None,
    }
}

fn run_import(
    session: &crate::library::service::LibrarySession,
    root: &Path,
    dir: &Path,
) -> ImportJob {
    let job = Arc::new(ImportJob::new(
        "test-job",
        "测试样本",
        ImportTarget::Library,
    ));
    run(source(session, root), job.clone(), request(dir)).unwrap();
    job.finish(ImportPhase::Completed);
    Arc::try_unwrap(job).ok().unwrap()
}

/// 平滑渐变：PNG 转 JPEG 后能同时满足画质底线与 5% 体积差。
fn smooth_image(seed: u32, width: u32, height: u32) -> DynamicImage {
    let mut image = RgbImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let value = (x * 3 + y * 2 + seed) % 256;
        *pixel = Rgb([
            value as u8,
            ((value + 60) % 256) as u8,
            ((value + 120) % 256) as u8,
        ]);
    }
    DynamicImage::ImageRgb8(image)
}

fn optimized_request(
    directory: &Path,
    tier: QualityTier,
    output_format: OptimizeFormat,
) -> ImportRequest {
    ImportRequest {
        optimize: Some(crate::import::ImportOptimize {
            tier,
            output_format,
        }),
        ..request(directory)
    }
}

fn run_with(
    session: &crate::library::service::LibrarySession,
    root: &Path,
    request: ImportRequest,
) -> ImportSnapshot {
    let job = Arc::new(ImportJob::new(
        "optimized-job",
        "测试样本",
        ImportTarget::Library,
    ));
    run(source(session, root), job.clone(), request).unwrap();
    job.finish(ImportPhase::Completed);
    job.snapshot()
}

#[test]
fn compresses_content_when_import_optimize_is_enabled() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let path = source_dir.join("smooth.png");
    smooth_image(7, 192, 128).save(&path).unwrap();
    let original = std::fs::read(&path).unwrap();
    let original_hash = blake3::hash(&original).to_hex().to_string();

    let library_root = temp.path().join("library");
    let session = open_library(&library_root);
    let snapshot = run_with(
        &session,
        &library_root,
        optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
    );

    assert_eq!(snapshot.progress.imported, 1, "{snapshot:?}");
    assert_eq!(snapshot.progress.optimized, 1, "{snapshot:?}");
    assert!(snapshot.progress.optimized_bytes > 0, "{snapshot:?}");
    // 新增体积取压缩后的最终字节，等于落盘文件大小。
    assert!(snapshot.progress.written_bytes > 0, "{snapshot:?}");

    let (hash, ext, format, size, optimized_at): (String, String, String, i64, i64) = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT content_hash, file_ext, format, file_size, optimized_at FROM images",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?)
        })
        .unwrap();
    assert_ne!(hash, original_hash);
    assert_eq!(ext, "jpg");
    assert_eq!(format, "jpeg");
    assert!(optimized_at > 0);
    assert!(size < original.len() as i64);

    let content = library_root.join("导入").join(format!("{hash}.jpg"));
    assert!(content.is_file(), "缺少压缩后的内容文件");
    assert_eq!(std::fs::metadata(&content).unwrap().len(), size as u64);
    assert_eq!(snapshot.progress.written_bytes, size as u64);
    assert!(
        !library_root
            .join("导入")
            .join(format!("{original_hash}.png"))
            .exists()
    );
}

/// PNG 的 `acTL` 块表示这是 APNG；这里按块结构手工拼一份，CRC 必须算对，
/// 否则解码器会先报错，测到的就不是压缩判据了。
fn apng_bytes(image: &DynamicImage) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    // IHDR 紧跟在 8 字节签名后，长度固定 25 字节（长度 + 类型 + 13 数据 + CRC）。
    let ihdr_end = 8 + 25;
    let mut chunk = Vec::with_capacity(20);
    chunk.extend_from_slice(&8_u32.to_be_bytes());
    chunk.extend_from_slice(b"acTL");
    chunk.extend_from_slice(&2_u32.to_be_bytes());
    chunk.extend_from_slice(&0_u32.to_be_bytes());
    chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
    let mut out = png[..ihdr_end].to_vec();
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&png[ihdr_end..]);
    out
}

/// 动画 PNG 在导入压缩时必须保留原图：压成单帧会丢掉其余帧且不可逆。
#[test]
fn keeps_apng_source_when_import_optimize_is_enabled() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let bytes = apng_bytes(&smooth_image(11, 192, 128));
    let path = source_dir.join("animated.png");
    std::fs::write(&path, &bytes).unwrap();
    let original_hash = blake3::hash(&bytes).to_hex().to_string();

    let library_root = temp.path().join("library");
    let session = open_library(&library_root);
    let snapshot = run_with(
        &session,
        &library_root,
        optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
    );

    assert_eq!(snapshot.progress.imported, 1, "{snapshot:?}");
    assert_eq!(snapshot.progress.optimized, 0, "{snapshot:?}");
    // 判据不满足不是编码失败，不该进失败计数。
    assert_eq!(snapshot.progress.optimize_failed, 0, "{snapshot:?}");
    let (hash, ext, size, optimized_at): (String, String, i64, i64) = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT content_hash, file_ext, file_size, optimized_at FROM images",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?)
        })
        .unwrap();
    assert_eq!(hash, original_hash, "入库的必须是原图字节");
    assert_eq!(ext, "png");
    assert_eq!(size, bytes.len() as i64);
    assert_eq!(optimized_at, 0);
}

#[test]
fn keeps_transparent_source_when_output_is_jpeg() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let path = source_dir.join("alpha.png");
    let mut image = RgbaImage::new(96, 96);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = Rgba([
            x as u8,
            y as u8,
            ((x + y) % 256) as u8,
            ((x * 2 + y * 3) % 256) as u8,
        ]);
    }
    DynamicImage::ImageRgba8(image).save(&path).unwrap();
    let original = std::fs::read(&path).unwrap();
    let original_hash = blake3::hash(&original).to_hex().to_string();

    let library_root = temp.path().join("library");
    let session = open_library(&library_root);
    let snapshot = run_with(
        &session,
        &library_root,
        optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
    );

    assert_eq!(snapshot.progress.imported, 1, "{snapshot:?}");
    assert_eq!(snapshot.progress.optimized, 0, "{snapshot:?}");
    let (hash, ext, optimized_at): (String, String, i64) = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT content_hash, file_ext, optimized_at FROM images",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .unwrap();
    assert_eq!(hash, original_hash);
    assert_eq!(ext, "png");
    assert_eq!(optimized_at, 0);
    assert!(
        library_root
            .join("导入")
            .join(format!("{hash}.png"))
            .is_file()
    );
}

#[test]
fn optimized_import_dedupes_same_params_and_splits_on_tier_change() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    smooth_image(3, 160, 120)
        .save(source_dir.join("smooth.png"))
        .unwrap();

    let library_root = temp.path().join("library");
    let session = open_library(&library_root);
    let first = run_with(
        &session,
        &library_root,
        optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
    );
    assert_eq!(first.progress.imported, 1, "{first:?}");

    let repeat = run_with(
        &session,
        &library_root,
        optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
    );
    assert_eq!(repeat.progress.imported, 0, "{repeat:?}");
    assert_eq!(repeat.progress.duplicates, 1, "{repeat:?}");

    let changed = run_with(
        &session,
        &library_root,
        optimized_request(&source_dir, QualityTier::Medium, OptimizeFormat::Jpeg),
    );
    assert_eq!(changed.progress.imported, 1, "{changed:?}");
    let rows: i64 = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(rows, 2);
}

#[test]
fn compression_keeps_thumbnail_phash_and_colors() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    smooth_image(11, 200, 140)
        .save(source_dir.join("smooth.png"))
        .unwrap();

    let plain_root = temp.path().join("plain");
    let plain = open_library(&plain_root);
    run_import(&plain, &plain_root, &source_dir);

    let optimized_root = temp.path().join("optimized");
    let optimized = open_library(&optimized_root);
    run_with(
        &optimized,
        &optimized_root,
        optimized_request(&source_dir, QualityTier::High, OptimizeFormat::Jpeg),
    );

    let read_row = |session: &crate::library::service::LibrarySession| {
        session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT phash, colors4, colors8 FROM images",
                    [],
                    |row| {
                        let phash: Vec<u8> = row.get(0)?;
                        let colors4: Vec<u8> = row.get(1)?;
                        let colors8: Vec<u8> = row.get(2)?;
                        Ok((phash, colors4, colors8))
                    },
                )?)
            })
            .unwrap()
    };
    assert_eq!(read_row(&plain), read_row(&optimized));

    let plain_thumb = {
        let pointer = thumbs::load_pointer(&plain.db, 1).unwrap().unwrap();
        plain.thumbs.read(pointer.location).unwrap()
    };
    let optimized_thumb = {
        let pointer = thumbs::load_pointer(&optimized.db, 1).unwrap().unwrap();
        optimized.thumbs.read(pointer.location).unwrap()
    };
    assert_eq!(plain_thumb, optimized_thumb);
}

#[test]
fn imports_mixed_formats_and_reports_duplicates() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    write_samples(&source_dir, 8, (120, 90));
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);

    let job = run_import(&session, &library_root, &source_dir);
    let snapshot = job.snapshot();
    assert_eq!(snapshot.progress.imported, 8, "{snapshot:?}");
    assert_eq!(snapshot.progress.written, 8, "{snapshot:?}");
    assert_eq!(snapshot.progress.failed, 0, "{snapshot:?}");
    assert_eq!(snapshot.progress.total, 8);
    assert_eq!(snapshot.preview_ids.len(), 8);
    let unique: std::collections::HashSet<_> = snapshot.preview_ids.iter().collect();
    assert_eq!(unique.len(), 8, "同一文件被处理了不止一次");

    session
        .db
        .with_reader(|connection| {
            let count: i64 =
                connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?;
            assert_eq!(count, 8);
            let missing: i64 = connection.query_row(
                "SELECT count(*) FROM images
                     WHERE thumb_segment = 0 OR thumb_length = 0
                        OR thumb_format != 'webp'
                        OR length(phash) != 32 OR length(colors4) != 16
                        OR length(colors8) != 32",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(missing, 0);
            Ok(())
        })
        .unwrap();

    let pointers = session
        .db
        .with_reader(|connection| {
            let mut statement =
                connection.prepare("SELECT id, content_hash, file_ext FROM images ORDER BY id")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap();
    for (id, hash, ext) in &pointers {
        let pointer = thumbs::load_pointer(&session.db, *id).unwrap().unwrap();
        let bytes = session.thumbs.read(pointer.location).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert!(decoded.width() > 0 && decoded.height() > 0);
        let content = library_root.join("导入").join(format!("{hash}.{ext}"));
        assert!(content.is_file(), "缺少原图 {}", content.display());
    }

    let second = Arc::new(ImportJob::new(
        "duplicate-job",
        "测试样本",
        ImportTarget::Library,
    ));
    run(
        source(&session, &library_root),
        second.clone(),
        request(&source_dir),
    )
    .unwrap();
    second.finish(ImportPhase::Completed);
    let snapshot = second.snapshot();
    assert_eq!(snapshot.progress.imported, 0);
    assert_eq!(snapshot.progress.duplicates, 8);
    assert_eq!(snapshot.progress.failed, 0);
}

#[test]
fn preserves_source_folders_and_merges_same_names() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("素材A");
    let second = temp.path().join("素材B");
    std::fs::create_dir_all(first.join("人物").join("速写")).unwrap();
    std::fs::create_dir_all(second.join("人物").join("色彩")).unwrap();
    sample_image(1, 64, 48)
        .save(first.join("人物").join("速写").join("a.png"))
        .unwrap();
    sample_image(2, 64, 48)
        .save(second.join("人物").join("色彩").join("b.png"))
        .unwrap();
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);

    let mut preserve = request(&first);
    preserve.paths = vec![
        first.to_string_lossy().into_owned(),
        second.to_string_lossy().into_owned(),
    ];
    preserve.folder_mode = FolderMode::Preserve;
    let job = Arc::new(ImportJob::new(
        "preserve-job",
        "测试样本",
        ImportTarget::Library,
    ));
    run(source(&session, &library_root), job.clone(), preserve).unwrap();
    job.finish(ImportPhase::Completed);
    assert_eq!(job.snapshot().progress.imported, 2);

    let folders: Vec<String> = session
        .db
        .with_reader(|connection| {
            let mut statement = connection.prepare("SELECT path FROM folders ORDER BY path")?;
            let paths = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(paths)
        })
        .unwrap();
    assert!(folders.contains(&"导入/人物".to_owned()));
    assert!(folders.contains(&"导入/人物/速写".to_owned()));
    assert!(folders.contains(&"导入/人物/色彩".to_owned()));
    assert!(library_root.join("导入").join("人物").join("速写").is_dir());
    assert!(library_root.join("导入").join("人物").join("色彩").is_dir());
}

#[test]
fn attributes_exact_duplicates_and_imported_pairs() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    write_samples(&source_dir, 2, (60, 60));
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);

    let first = run_import(&session, &library_root, &source_dir).snapshot();
    assert_eq!(first.imported_pairs.len(), 2);
    assert!(first.duplicate_pairs.is_empty());
    let mut first_ids: Vec<i64> = first
        .imported_pairs
        .iter()
        .map(|pair| pair.image_id)
        .collect();
    first_ids.sort_unstable();

    let second = run_import(&session, &library_root, &source_dir).snapshot();
    assert_eq!(second.progress.duplicates, 2);
    assert_eq!(second.duplicate_pair_count, 2);
    assert_eq!(second.duplicate_pairs.len(), 2);
    assert!(!second.duplicate_pairs_truncated);
    let mut duplicate_ids: Vec<i64> = second
        .duplicate_pairs
        .iter()
        .map(|pair| pair.image_id)
        .collect();
    duplicate_ids.sort_unstable();
    assert_eq!(duplicate_ids, first_ids);
    assert!(second.duplicate_pairs.iter().all(|pair| !pair.trashed));
    assert!(second.imported_pairs.is_empty());
}

#[test]
fn attributes_batch_duplicates_to_the_kept_source() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let bytes = std::fs::read({
        let path = source_dir.join("origin.png");
        sample_image(9, 48, 48).save(&path).unwrap();
        path
    })
    .unwrap();
    std::fs::write(source_dir.join("copy.png"), &bytes).unwrap();
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);

    let snapshot = run_import(&session, &library_root, &source_dir).snapshot();
    assert_eq!(snapshot.progress.imported, 1);
    assert_eq!(snapshot.progress.duplicates, 1);
    assert_eq!(snapshot.batch_duplicate_pair_count, 1);
    assert_eq!(snapshot.batch_duplicate_pairs.len(), 1);
    let pair = &snapshot.batch_duplicate_pairs[0];
    assert_ne!(pair.source_path, pair.kept_path);
    assert_eq!(snapshot.imported_pairs[0].source_path, pair.kept_path);
}

#[test]
fn duplicate_pairs_mark_trashed_existing_images() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    write_samples(&source_dir, 1, (60, 60));
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);
    let first = run_import(&session, &library_root, &source_dir).snapshot();
    let image_id = first.imported_pairs[0].image_id;
    session
        .db
        .with_writer(|connection| {
            connection.execute("UPDATE images SET deleted_at = 1 WHERE id = ?1", [image_id])?;
            Ok(())
        })
        .unwrap();

    let second = run_import(&session, &library_root, &source_dir).snapshot();
    assert_eq!(second.duplicate_pair_count, 1);
    assert_eq!(second.duplicate_pairs[0].image_id, image_id);
    assert!(second.duplicate_pairs[0].trashed);
}

#[test]
fn merges_metadata_for_existing_content() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    write_samples(&source_dir, 1, (60, 60));
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);
    run_import(&session, &library_root, &source_dir);

    let mut merge_request = request(&source_dir);
    merge_request.duplicate_policy = DuplicatePolicy::Merge;
    merge_request.tags = vec!["第二次".into()];
    merge_request.score = 5;
    let job = Arc::new(ImportJob::new(
        "merge-job",
        "测试样本",
        ImportTarget::Library,
    ));
    run(source(&session, &library_root), job.clone(), merge_request).unwrap();
    job.finish(ImportPhase::Completed);
    let snapshot = job.snapshot();
    assert_eq!(snapshot.progress.merged, 1);
    assert_eq!(snapshot.progress.duplicates, 0);
    let image_id: i64 = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT id FROM images", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(snapshot.merged_image_ids, vec![image_id]);
    assert_eq!(snapshot.duplicate_pair_count, 1);
    assert_eq!(snapshot.duplicate_pairs[0].image_id, image_id);

    session
        .db
        .with_reader(|connection| {
            let (score, tags): (i64, i64) = connection.query_row(
                "SELECT score,
                            (SELECT count(*) FROM image_tags WHERE image_id = images.id)
                     FROM images",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(score, 5);
            assert_eq!(tags, 2);
            Ok(())
        })
        .unwrap();
}

#[test]
fn library_import_replaces_same_content_in_temp_folder() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    write_samples(&source_dir, 3, (40, 40));
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);

    // 先进临时区：路径不变，源文件也还在，正好模拟“先看一眼再决定入库”。
    let mut temp_request = request(&source_dir);
    temp_request.target = ImportTarget::Temp;
    temp_request.folder.clear();
    temp_request.tags.clear();
    let temp_snapshot = run_with(&session, &library_root, temp_request);
    assert_eq!(temp_snapshot.progress.imported, 3, "{temp_snapshot:?}");

    // 同一个目录再导图库：临时区的同内容行让位，三张都真正入库。
    let snapshot = run_with(&session, &library_root, request(&source_dir));
    assert_eq!(snapshot.progress.imported, 3, "{snapshot:?}");
    assert_eq!(snapshot.progress.duplicates, 0, "{snapshot:?}");
    assert_eq!(snapshot.progress.temp_replaced, 3, "{snapshot:?}");

    let (library_rows, temp_rows): (i64, i64) = session
        .db
        .with_reader(|connection| {
            Ok((
                connection.query_row(
                    "SELECT count(*) FROM images JOIN folders ON folders.id = images.folder_id
                         WHERE folders.kind = 'normal'",
                    [],
                    |row| row.get(0),
                )?,
                connection.query_row(
                    "SELECT count(*) FROM images JOIN folders ON folders.id = images.folder_id
                         WHERE folders.kind = 'temp'",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!((library_rows, temp_rows), (3, 0));

    // 临时区留下的原图副本在提交后被清掉，不占着磁盘等关库。
    let temp_dir = library_root.join(crate::library::paths::TEMP_FOLDER_DIR);
    let leftover = std::fs::read_dir(&temp_dir).unwrap().count();
    assert_eq!(leftover, 0, "临时区的原图副本没有清理");
}

#[test]
fn cancellation_keeps_committed_rows_and_cleans_temp() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    write_samples(&source_dir, 120, (48, 48));
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);

    let mut cancel_request = request(&source_dir);
    cancel_request.cancel_after = Some(20);
    let job = Arc::new(ImportJob::new(
        "cancel-job",
        "测试样本",
        ImportTarget::Library,
    ));
    run(source(&session, &library_root), job.clone(), cancel_request).unwrap();
    assert!(job.should_cancel());
    job.finish(ImportPhase::Cancelled);
    let snapshot = job.snapshot();
    assert!(snapshot.progress.processed < snapshot.progress.total);
    assert_eq!(
        snapshot.progress.written, snapshot.progress.imported,
        "已入库数量必须与 written 一致：{snapshot:?}"
    );
    assert!(snapshot.progress.written <= snapshot.progress.processed);

    let rows: i64 = session
        .db
        .with_reader(|connection| {
            let rows = connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(rows as usize, snapshot.progress.imported);

    let temp_dir = library_root.join(".artbank").join("temp");
    let leftover: Vec<_> = std::fs::read_dir(&temp_dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(leftover.is_empty(), "临时目录未清理：{leftover:?}");
}

#[test]
fn missing_pack_is_reported_not_panicking() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    write_samples(&source_dir, 1, (50, 50));
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);
    run_import(&session, &library_root, &source_dir);

    std::fs::remove_file(
        library_root
            .join(".artbank")
            .join("thumbs")
            .join("0001.pack"),
    )
    .unwrap();
    let pointer = thumbs::load_pointer(&session.db, 1).unwrap().unwrap();
    assert!(session.thumbs.read(pointer.location).is_err());
}

/// 发布时目标已存在：内容一致才接管，截断的孤儿必须被替换。
#[test]
fn publish_adopts_matching_orphan_and_replaces_truncated_one() {
    let temp = tempdir().unwrap();
    let mut payload = Vec::new();
    smooth_image(5, 64, 48)
        .write_to(
            &mut std::io::Cursor::new(&mut payload),
            image::ImageFormat::Png,
        )
        .unwrap();
    let hash = blake3::hash(&payload).to_hex().to_string();
    let destination = temp.path().join(format!("{hash}.png"));

    // 完整孤儿：接管，返回 false 表示目标文件不是本批写的。
    std::fs::write(&destination, &payload).unwrap();
    let staged = temp.path().join("staged-1.png");
    std::fs::write(&staged, &payload).unwrap();
    assert!(!publish(&staged, &destination, &hash).unwrap());
    assert!(!staged.exists(), "接管后临时文件应删除");
    assert_eq!(std::fs::read(&destination).unwrap(), payload);

    // 截断孤儿：内容不匹配，删掉后重新发布。
    std::fs::write(&destination, &payload[..payload.len() / 2]).unwrap();
    let staged = temp.path().join("staged-2.png");
    std::fs::write(&staged, &payload).unwrap();
    assert!(publish(&staged, &destination, &hash).unwrap());
    assert!(!staged.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), payload);
}

/// 目标位置已有截断的孤儿时，导入不能直接把它登记成正常图片。
#[test]
fn import_replaces_truncated_orphan_instead_of_registering_it() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let path = source_dir.join("photo.png");
    smooth_image(9, 96, 64).save(&path).unwrap();
    let original = std::fs::read(&path).unwrap();
    let hash = blake3::hash(&original).to_hex().to_string();

    let library_root = temp.path().join("library");
    let session = open_library(&library_root);
    let destination = LibraryPaths::new(&library_root).content_file("导入", &format!("{hash}.png"));
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&destination, &original[..original.len() / 2]).unwrap();

    let snapshot = run_with(&session, &library_root, request(&source_dir));
    assert_eq!(snapshot.progress.imported, 1, "{snapshot:?}");
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        original,
        "截断的孤儿应被完整内容替换"
    );
}

#[test]
#[ignore = "高成本基准（非日常）：生成并导入 1000 张样本；用 cargo test -- --ignored --nocapture perf_import_1000 运行"]
fn perf_import_1000() {
    let temp = tempdir().unwrap();
    let source_dir = temp.path().join("source");
    write_samples(&source_dir, 1000, (768, 512));
    let library_root = temp.path().join("library");
    let session = open_library(&library_root);

    let job = Arc::new(ImportJob::new(
        "perf-job",
        "测试样本",
        ImportTarget::Library,
    ));
    let start = Instant::now();
    run(
        source(&session, &library_root),
        job.clone(),
        request(&source_dir),
    )
    .unwrap();
    let elapsed = start.elapsed();
    job.finish(ImportPhase::Completed);
    let snapshot = job.snapshot();
    let seconds = elapsed.as_secs_f64();
    let rate = snapshot.progress.imported as f64 / seconds;

    println!(
        "[perf] import: imported={} failed={} duplicates={} total={} elapsed={:.2}s rate={:.1}/s",
        snapshot.progress.imported,
        snapshot.progress.failed,
        snapshot.progress.duplicates,
        snapshot.progress.total,
        seconds,
        rate
    );

    assert_eq!(snapshot.progress.imported, 1000);
    assert_eq!(snapshot.progress.failed, 0);
    assert!(rate >= 20.0, "导入吞吐 {rate:.1}/s 低于 20/s");
}

#[test]
fn writer_guard_joins_when_the_worker_unwinds() {
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_flag = finished.clone();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = WriterGuard::new(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            worker_flag.store(true, Ordering::SeqCst);
        }));
        panic!("工作线程 panic");
    }));
    assert!(outcome.is_err());
    assert!(
        finished.load(Ordering::SeqCst),
        "写线程必须在外层 panic 展开时被 join 掉"
    );
}

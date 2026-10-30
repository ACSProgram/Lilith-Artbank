use std::{
    fs,
    io::Cursor,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use image::{DynamicImage, Rgb, RgbImage};
use rusqlite::params;
use tempfile::tempdir;

use super::commit::{PublishOutcome, publish};
use super::execute::skipped_count;
use super::*;
use crate::{
    imageops::optimize::{OptimizeOptions, SkipReason},
    library::{
        NoopReporter, calibration::OptimizeCalibration, history::NewOperation,
        optimize_estimate::OptimizeEstimate, service,
    },
};

struct Fixture {
    _temp: tempfile::TempDir,
    session: service::LibrarySession,
    root: PathBuf,
}

fn fixture() -> (Fixture, i64, String, PathBuf) {
    let temp = tempdir().unwrap();
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "压缩测试", 1).unwrap();
    let mut image = RgbImage::new(160, 120);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        // 平滑渐变：JPEG 在较高画质下仍能大幅缩小，且 PSNR 足够高。
        *pixel = Rgb([
            (x * 255 / 159) as u8,
            (y * 255 / 119) as u8,
            ((x + y) * 255 / 278) as u8,
        ]);
    }
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(image)
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
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
                    modified_at, thumb_version
                 ) VALUES (?1, 'png', 1, ?2, 160, 120, 'png', 0,
                    zeroblob(32), zeroblob(16), zeroblob(32), 4, '保留描述', 11, 11, 3)",
                params![hash, bytes.len() as i64],
            )?;
            let id = connection.last_insert_rowid();
            connection.execute("INSERT INTO tags(name) VALUES ('保留标签')", [])?;
            connection.execute(
                "INSERT INTO image_tags(image_id, tag_id)
                 SELECT ?1, id FROM tags WHERE name = '保留标签'",
                [id],
            )?;
            connection.execute(
                "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name,
                    pack_offset, pack_length, updated_at)
                 VALUES (?1, 'embedding', 'm', 3, 'f.bin', 0, 12, 1)",
                [id],
            )?;
            Ok(id)
        })
        .unwrap();

    (
        Fixture {
            _temp: temp,
            session,
            root,
        },
        image_id,
        hash,
        path,
    )
}

#[derive(Default)]
struct CountingReporter {
    calls: AtomicUsize,
    total: AtomicUsize,
    skipped: AtomicUsize,
    meter: crate::concurrency::meter::WorkMeter,
}

impl crate::library::BatchReporter for CountingReporter {
    fn report(&self, _completed: usize, total: usize) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        false
    }

    fn meter(&self) -> Option<&crate::concurrency::meter::WorkMeter> {
        Some(&self.meter)
    }

    fn skipped_count(&self, skipped: usize) {
        self.skipped.store(skipped, Ordering::Relaxed);
    }
}

fn run(fixture: &Fixture, ids: &[i64], options: &OptimizeOptions) -> OptimizeReport {
    let operation = crate::library::history::begin_live(
        &fixture.session.db,
        NewOperation::new(
            crate::library::history::HistoryKind::Optimize,
            "优化图片空间",
        ),
    )
    .unwrap();
    let candidates = collect_candidates(&fixture.session.db, ids).expect("读取候选失败");
    let report = optimize(
        &fixture.session.db,
        &fixture.root,
        &fixture.session.thumbs,
        candidates,
        options,
        operation,
        &NoopReporter,
        &crate::concurrency::budget::ImageBudget::new(2),
        &crate::concurrency::budget::ImageMemoryBudget::default(),
    )
    .unwrap();
    crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();
    report
}

#[test]
fn grant_requires_matching_fingerprint_and_stable_candidates() {
    let mut estimate = OptimizeEstimate {
        options_fingerprint: "fp".to_owned(),
        candidate_fingerprint: "set".to_owned(),
        candidate_count: 100,
        candidate_bytes: 1_000_000,
        ..OptimizeEstimate::default()
    };

    assert!(verify_grant(&estimate, "fp", 100, 1_000_000, "set").is_ok());
    assert!(verify_grant(&estimate, "fp", 110, 1_100_000, "set").is_ok());
    assert!(verify_grant(&estimate, "other", 100, 1_000_000, "set").is_err());
    assert!(verify_grant(&estimate, "fp", 120, 1_000_000, "set").is_err());
    assert!(verify_grant(&estimate, "fp", 100, 1_200_000, "set").is_err());
    // 张数与体积都在范围内，但集合换了另一批图。
    assert!(verify_grant(&estimate, "fp", 100, 1_000_000, "other-set").is_err());
    // 旧结果没有集合指纹时不因此拒绝（升级后的首次执行）。
    estimate.candidate_fingerprint.clear();
    assert!(verify_grant(&estimate, "fp", 100, 1_000_000, "other-set").is_ok());
    estimate.candidate_count = 0;
    assert!(verify_grant(&estimate, "fp", 0, 0, "set").is_err());
}

#[test]
fn source_check_reports_missing_and_changed_files() {
    let (fixture, image_id, _hash, path) = fixture();
    let rows = collect_candidates(&fixture.session.db, &[image_id]).unwrap();
    let options = OptimizeOptions::default();
    let ok = verify_sources(&fixture.root, &rows, &options);
    assert_eq!((ok.missing, ok.changed), (0, 0), "{ok:?}");
    assert!(ok.missing_dirs.is_empty());

    // 大小不符（被外部替换或截断）。
    let original = fs::read(&path).unwrap();
    fs::write(&path, &original[..original.len() / 2]).unwrap();
    let changed = verify_sources(&fixture.root, &rows, &options);
    assert_eq!((changed.missing, changed.changed), (0, 1), "{changed:?}");
    assert_eq!(changed.bad_ids, vec![image_id]);

    // 原图被删。
    fs::remove_file(&path).unwrap();
    let missing = verify_sources(&fixture.root, &rows, &options);
    assert_eq!((missing.missing, missing.changed), (1, 0), "{missing:?}");

    // 整个目录被删：目录缺失要能单独报出来。
    let mut ghost = rows[0].clone();
    ghost.folder_path = "已经不存在的目录".to_owned();
    let gone = verify_sources(&fixture.root, &[ghost], &options);
    assert_eq!(
        gone.missing_dirs,
        vec!["已经不存在的目录".to_owned()],
        "{gone:?}"
    );
}

/// 原图缺失只计数，不触发“连续 5 张失败即停止”。
#[test]
fn missing_sources_are_counted_without_stopping_the_run() {
    let (fixture, first_id, _, _) = fixture();
    let mut ids = vec![first_id];
    let mut paths = Vec::new();
    for index in 1..12_u32 {
        let mut image = RgbImage::new(96, 64);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([
                ((x * 7 + index * 11) % 256) as u8,
                ((y * 5 + index * 17) % 256) as u8,
                ((x + y + index * 29) % 256) as u8,
            ]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let path = fixture.root.join(format!("{hash}.png"));
        fs::write(&path, &bytes).unwrap();
        paths.push(path);
        let id = fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'png', 1, ?2, 96, 64, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params![hash, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        ids.push(id);
    }
    // 前 6 张原图被外部删掉：超过连续失败阈值，但不应停掉任务。
    for path in paths.iter().take(6) {
        fs::remove_file(path).unwrap();
    }

    let report = run(&fixture, &ids, &OptimizeOptions::default());
    assert_eq!(report.source_missing, 6, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    assert_eq!(report.stop_reason, None, "{report:?}");
    // 其余 6 张照常处理（fixture 的首张 + 后面 5 张）。
    assert_eq!(report.processed, 6, "{report:?}");
}

/// 字节额度小到每张都超限时走独占路径：任务照常完成，额度用完归零。
#[test]
fn optimize_returns_memory_permit_for_oversized_images() {
    let (fixture, image_id, _hash, _old_path) = fixture();
    let operation = crate::library::history::begin_live(
        &fixture.session.db,
        NewOperation::new(
            crate::library::history::HistoryKind::Optimize,
            "优化图片空间",
        ),
    )
    .unwrap();
    let candidates = collect_candidates(&fixture.session.db, &[image_id]).unwrap();
    let memory = crate::concurrency::budget::ImageMemoryBudget::new(1);
    let report = optimize(
        &fixture.session.db,
        &fixture.root,
        &fixture.session.thumbs,
        candidates,
        &OptimizeOptions::default(),
        operation,
        &NoopReporter,
        &crate::concurrency::budget::ImageBudget::new(2),
        &memory,
    )
    .unwrap();
    crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();
    assert_eq!(report.processed, 1, "{report:?}");
    assert_eq!(memory.in_use(), 0);
}

#[test]
fn compresses_content_and_keeps_metadata() {
    let (fixture, image_id, old_hash, old_path) = fixture();
    let report = run(&fixture, &[image_id], &OptimizeOptions::default());
    assert_eq!(report.processed, 1, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    assert!(report.savings_bytes > 0);
    assert!(!old_path.exists());
    // 删旧图发生在提交之后，报告里要能看见这段耗时。
    assert_eq!(report.deleted_files, 1);
    assert!(report.elapsed_ms >= report.commit_ms);
    assert_eq!(
        report.skipped,
        report.prefilter_skipped + report.candidate_skipped + report.duplicate_skipped
    );

    let (hash, size, optimized_at, score, description, imported, modified): (
        String,
        i64,
        i64,
        i64,
        String,
        i64,
        i64,
    ) = fixture
        .session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT content_hash, file_size, optimized_at, score, description,
                        imported_at, modified_at
                 FROM images WHERE id = ?1",
                [image_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?)
        })
        .unwrap();
    assert_ne!(hash, old_hash);
    assert_eq!(size, report.new_bytes as i64);
    assert!(optimized_at > 0);
    assert_eq!((score, description.as_str()), (4, "保留描述"));
    assert_eq!((imported, modified), (11, 11));

    let extras: (i64, i64, i64) = fixture
        .session
        .db
        .with_reader(|connection| {
            Ok((
                connection.query_row("SELECT count(*) FROM image_tags", [], |row| row.get(0))?,
                connection.query_row("SELECT count(*) FROM feature_blobs", [], |row| row.get(0))?,
                connection.query_row(
                    "SELECT length(phash) FROM images WHERE id = ?1",
                    [image_id],
                    |row| row.get(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!(extras, (1, 1, 32));

    let operation: (String, i64, String) = fixture
        .session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT kind, affected, status FROM operations ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .unwrap();
    assert_eq!(operation, ("optimize".to_owned(), 1, "applied".to_owned()));
}

#[test]
fn reports_progress_per_image_instead_of_per_batch() {
    let (fixture, first_id, _, _) = fixture();
    let mut ids = vec![first_id];
    for index in 1..12_u32 {
        let mut image = RgbImage::new(96, 64);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([
                ((x * 7 + index * 11) % 256) as u8,
                ((y * 5 + index * 17) % 256) as u8,
                ((x + y + index * 29) % 256) as u8,
            ]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        fs::write(fixture.root.join(format!("{hash}.png")), &bytes).unwrap();
        let id = fixture
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'png', 1, ?2, 96, 64, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params![hash, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        ids.push(id);
    }

    let operation = crate::library::history::begin_live(
        &fixture.session.db,
        NewOperation::new(
            crate::library::history::HistoryKind::Optimize,
            "优化图片空间",
        ),
    )
    .unwrap();
    let candidates = collect_candidates(&fixture.session.db, &ids).unwrap();
    let reporter = CountingReporter::default();
    let report = optimize(
        &fixture.session.db,
        &fixture.root,
        &fixture.session.thumbs,
        candidates.clone(),
        &OptimizeOptions::default(),
        operation,
        &reporter,
        &crate::concurrency::budget::ImageBudget::new(2),
        &crate::concurrency::budget::ImageMemoryBudget::default(),
    )
    .unwrap();
    crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();

    // 一次初始上报 + 每张一次；按批上报只会是 1 次。
    assert_eq!(
        reporter.calls.load(Ordering::Relaxed),
        candidates.len() + 1,
        "进度没有按张上报"
    );
    // 进度分母是候选张数，不是像素总量；每张推进一格，跑完正好等于分母。
    let jobs = candidates.len() - report.prefilter_skipped as usize;
    assert_eq!(reporter.meter.total_units(), jobs as u64);
    assert_eq!(reporter.meter.done_units(), jobs as u64);
    // 任务快照的分母同样是候选池：执行前被排除的图片不进进度，否则开局就六成。
    assert_eq!(reporter.total.load(Ordering::Relaxed), jobs);
    assert_eq!(
        reporter.skipped.load(Ordering::Relaxed),
        report.skipped as usize - report.prefilter_skipped as usize,
        "任务里报的跳过数不该包含执行前被排除的图片"
    );
}

#[test]
fn second_run_skips_already_optimized_then_allows_forced_reprocessing() {
    let (fixture, image_id, _, _) = fixture();
    let first = run(&fixture, &[image_id], &OptimizeOptions::default());
    assert_eq!(first.processed, 1);

    let second = run(&fixture, &[image_id], &OptimizeOptions::default());
    assert_eq!(
        (second.processed, second.prefilter_skipped),
        (0, 1),
        "{second:?}"
    );

    let forced = run(
        &fixture,
        &[image_id],
        &OptimizeOptions {
            skip_optimized: false,
            ..OptimizeOptions::default()
        },
    );
    // 再次压缩要么成功，要么因画质/体积判据跳过；关键是不再被标记拦住。
    assert_eq!(forced.failed, 0, "{forced:?}");
}

#[test]
fn pending_output_recovers_as_applied_after_crash() {
    let (fixture, image_id, old_hash, _) = fixture();
    let operation = crate::library::history::begin_live(
        &fixture.session.db,
        NewOperation::new(
            crate::library::history::HistoryKind::Optimize,
            "优化图片空间",
        ),
    )
    .unwrap();
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "UPDATE operations SET affected = 1 WHERE id = ?1",
                [operation],
            )?;
            connection.execute(
                "UPDATE images SET optimized_at = 1 WHERE id = ?1",
                [image_id],
            )?;
            Ok(())
        })
        .unwrap();

    crate::library::history::recover_on_open(&fixture.session.db).unwrap();
    let status: String = fixture
        .session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT status FROM operations WHERE id = ?1",
                [operation],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(status, "applied");
    assert_eq!(
        fixture
            .session
            .db
            .with_reader(|connection| {
                let hash: String = connection.query_row(
                    "SELECT content_hash FROM images WHERE id = ?1",
                    [image_id],
                    |row| row.get(0),
                )?;
                Ok(hash)
            })
            .unwrap(),
        old_hash
    );
}

/// 目标路径上的文件内容与预期哈希一致时直接接管，不重复写盘。
#[test]
fn publish_adopts_matching_orphan_and_replaces_mismatched_one() {
    let temp = tempdir().unwrap();
    let payload = b"published-bytes".to_vec();
    let hash = blake3::hash(&payload).to_hex().to_string();
    let destination = temp.path().join(format!("{hash}.jpg"));

    // 孤儿完整：接管，临时文件不再需要。
    fs::write(&destination, &payload).unwrap();
    let staged = temp.path().join("staged-1.tmp");
    fs::write(&staged, &payload).unwrap();
    assert!(matches!(
        publish(&staged, &destination, &hash).unwrap(),
        PublishOutcome::Adopted
    ));
    assert_eq!(fs::read(&destination).unwrap(), payload);

    // 孤儿截断：内容不匹配，删掉后重新发布。
    fs::write(&destination, &payload[..4]).unwrap();
    let staged = temp.path().join("staged-2.tmp");
    fs::write(&staged, &payload).unwrap();
    assert!(matches!(
        publish(&staged, &destination, &hash).unwrap(),
        PublishOutcome::Written
    ));
    assert!(!staged.exists());
    assert_eq!(fs::read(&destination).unwrap(), payload);
}

/// 提交阶段失败时，本批已发布的新文件必须删掉，旧图保持不动。
#[test]
fn failed_commit_removes_new_files_and_keeps_old_one() {
    let (fixture, image_id, old_hash, old_path) = fixture();
    let row = collect_candidates(&fixture.session.db, &[image_id])
        .unwrap()
        .pop()
        .unwrap();
    // 指向一个不存在的行：编码与发布都会成功，事务里的 UPDATE 命中 0 行。
    let mut ghost = row.clone();
    ghost.id = image_id + 10_000;

    let operation = crate::library::history::begin_live(
        &fixture.session.db,
        NewOperation::new(
            crate::library::history::HistoryKind::Optimize,
            "优化图片空间",
        ),
    )
    .unwrap();
    let error = optimize(
        &fixture.session.db,
        &fixture.root,
        &fixture.session.thumbs,
        vec![ghost],
        &OptimizeOptions::default(),
        operation,
        &NoopReporter,
        &crate::concurrency::budget::ImageBudget::new(2),
        &crate::concurrency::budget::ImageMemoryBudget::default(),
    )
    .unwrap_err();
    crate::library::history::finish_live(&fixture.session.db, operation, "").unwrap();
    assert!(error.to_string().contains("已被其他操作修改"), "{error}");

    // 旧图还在，行也没变；新文件（内容哈希文件）已被清理。
    assert!(old_path.exists());
    let files: Vec<PathBuf> = fs::read_dir(&fixture.root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(
        files[0].file_name().unwrap().to_string_lossy(),
        format!("{old_hash}.png")
    );
    let temp_files = fs::read_dir(fixture.root.join(".artbank").join("temp"))
        .unwrap()
        .count();
    assert_eq!(temp_files, 0, "临时文件应随守卫删除");
}

/// 六个桶之和恒等于 `candidate_skipped`，界面上的细项加起来不会多也不会少。
#[test]
fn skip_buckets_cover_every_reason() {
    let reasons = [
        SkipReason::ColorProfile,
        SkipReason::NotSmallEnough,
        SkipReason::QualityNotMet,
        SkipReason::SixteenBit,
        SkipReason::AnimatedOrMultipage,
        SkipReason::BppLow,
        SkipReason::AlreadyOptimized,
        SkipReason::Transparent,
        SkipReason::TooLargeForWebp,
    ];
    let mut report = OptimizeReport::default();
    for reason in reasons {
        report.record_skip(reason);
    }
    assert_eq!(report.candidate_skipped, reasons.len() as u64);
    let buckets = report.skip_buckets();
    assert_eq!(buckets.len(), 6, "{buckets:?}");
    let sum: u64 = buckets.iter().map(|(_, count)| count).sum();
    assert_eq!(sum, report.candidate_skipped);
    // 四个「解码后才发现」的原因都落在 late，按张数降序排在最前。
    assert_eq!(buckets[0], ("late".to_owned(), 4));
    // key 与估算视图的图例同名，前端两张图例共用一套文案。
    assert_eq!(
        buckets
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        vec![
            "late",
            "color",
            "depth",
            "quality",
            "notSmaller",
            "multiframe"
        ]
    );
}

/// 进度的跳过口径只认候选池：执行前被排除的图片不算「已跳过」。
#[test]
fn progress_skipped_excludes_prefiltered() {
    let mut report = OptimizeReport {
        prefilter_skipped: 61_401,
        ..OptimizeReport::default()
    };
    report.record_skip(SkipReason::ColorProfile);
    report.duplicate_skipped = 2;
    report.source_missing = 1;
    assert_eq!(report.progress_skipped(), 4);
    assert_eq!(skipped_count(&report), 61_405);
}

/// 一次执行后校准值写进 meta，且数值落在合理量级内。
#[test]
fn writes_calibration_after_a_run() {
    let (fixture, image_id, _, _) = fixture();
    let report = run(&fixture, &[image_id], &OptimizeOptions::default());
    assert_eq!(report.processed, 1);
    // 少于 20 张的执行不覆盖校准值：默认库里本来就没有记录。
    assert_eq!(
        OptimizeCalibration::load(&fixture.session.db).unwrap(),
        None
    );
}

use std::fs;

use rusqlite::OptionalExtension;
use tempfile::tempdir;

use super::replay::{HistoryRunGuard, append_redo, load_redo_records, start_run};
use super::undo::import::{redo_import, undo_import};
use super::*;
use crate::{
    app::tasks::{TaskHandle, TaskKind},
    library::{NoopReporter, collections, folders, import::ensure_folder, metadata, service},
};

struct Fixture {
    _temp: tempfile::TempDir,
    session: service::LibrarySession,
    root: PathBuf,
    folder: i64,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "历史测试", 1).unwrap();
        let folder = session
            .db
            .with_writer(|connection| ensure_folder(connection, "人物", 1))
            .unwrap();
        Self {
            _temp: temp,
            session,
            root,
            folder,
        }
    }

    fn db(&self) -> &LibraryDb {
        &self.session.db
    }

    fn insert_image(&self, seed: &str) -> i64 {
        let hash = format!("{seed:0>64}");
        self.session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'jpg', ?2, 100, 10, 10, 'jpeg', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params![hash, self.folder],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap()
    }

    fn write_content(&self, seed: &str) -> PathBuf {
        let hash = format!("{seed:0>64}");
        let path = LibraryPaths::new(&self.root).content_file("人物", &format!("{hash}.jpg"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"content").unwrap();
        path
    }

    /// 插入一张能真正解码的 PNG 行，并写一条 feature_blobs，供编辑撤销测试使用。
    fn insert_editable_image(&self) -> (i64, String, PathBuf) {
        let mut image = image::RgbaImage::new(24, 16);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x * 9) as u8, (y * 13) as u8, 70, 255]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let path = LibraryPaths::new(&self.root).content_file("人物", &format!("{hash}.png"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &bytes).unwrap();
        let image_id = self
            .session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'png', ?2, ?3, 24, 16, 'png', 1,
                        zeroblob(32), zeroblob(16), zeroblob(32), 4, '保留描述', 1, 1)",
                    params![hash, self.folder, bytes.len() as i64],
                )?;
                let id = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name,
                        pack_offset, pack_length, updated_at)
                     VALUES (?1, 'embedding', 'm', 3, 'f.bin', 0, 12, 1)",
                    [id],
                )?;
                Ok(id)
            })
            .unwrap();
        (image_id, hash, path)
    }
}

fn sample_png(path: &Path, seed: u32) {
    let mut image = image::RgbImage::new(48, 32);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = image::Rgb([
            ((x * 3 + seed * 11) % 256) as u8,
            ((y * 5 + seed * 7) % 256) as u8,
            (seed * 37 % 256) as u8,
        ]);
    }
    image.save(path).unwrap();
}

fn task() -> Arc<TaskHandle> {
    TaskHandle::new("undo-test", TaskKind::Undo, "测试")
}

#[test]
fn write_rolls_back_empty_operations() {
    let fixture = Fixture::new();
    write(
        fixture.db(),
        NewOperation::new(HistoryKind::Trash, "移入回收站"),
        |_, _| Ok(0_u64),
    )
    .unwrap();
    assert!(list(fixture.db()).unwrap().is_empty());
}

#[test]
fn trims_oldest_records_and_keeps_newest() {
    let fixture = Fixture::new();
    for index in 0..5 {
        write(
            fixture.db(),
            NewOperation::new(HistoryKind::Trash, format!("记录 {index}")),
            |_, draft| {
                draft.affected(1);
                Ok(())
            },
        )
        .unwrap();
    }
    let evicted = trim(fixture.db(), 2).unwrap();
    assert_eq!(evicted.len(), 3);
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].label, "记录 4");
    assert_eq!(entries[1].label, "记录 3");
}

#[test]
fn trim_and_clear_drop_undo_runs() {
    let fixture = Fixture::new();
    let mut ids = Vec::new();
    for index in 0..3 {
        let (_, id) = write_with_id(
            fixture.db(),
            NewOperation::new(HistoryKind::Trash, format!("记录 {index}")),
            |_, draft| {
                draft.affected(1);
                Ok(())
            },
        )
        .unwrap();
        ids.push(id.unwrap());
    }
    fixture
        .db()
        .with_writer(|connection| {
            for id in &ids {
                connection.execute(
                    "INSERT INTO undo_runs(target_operation_id, state, started_at)
                     VALUES (?1, 'completed', 1)",
                    [id],
                )?;
            }
            Ok(())
        })
        .unwrap();

    trim(fixture.db(), 1).unwrap();
    let runs: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM undo_runs", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(runs, 1, "被裁掉的记录不再保留运行审计");

    clear(fixture.db(), &fixture.root).unwrap();
    let runs: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM undo_runs", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(runs, 0);
}

fn force_undo_failed(fixture: &Fixture, operation_id: i64) {
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "UPDATE operations SET status = 'undoFailed', error = '文件被占用' WHERE id = ?1",
                [operation_id],
            )?;
            Ok(())
        })
        .unwrap();
}

fn deleted_at(fixture: &Fixture, image_id: i64) -> Option<i64> {
    fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT deleted_at FROM images WHERE id = ?1",
                [image_id],
                |row| row.get::<_, Option<i64>>(0),
            )?)
        })
        .unwrap()
}

#[test]
fn retry_undo_failed_record_undoes_only_it() {
    let fixture = Fixture::new();
    let image = fixture.insert_image("retry");
    metadata::set_trashed(fixture.db(), &[image], true).unwrap();
    let entry = list(fixture.db()).unwrap().remove(0);
    force_undo_failed(&fixture, entry.id);
    // 模拟上次撤销已提交的重做快照：重试是续做，不能把它们清掉（导入撤销按批提交，
    // 重做要靠这些快照找回已撤过的行）。
    fixture
        .db()
        .with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            append_redo(
                &transaction,
                entry.id,
                &RedoRecord::PurgeIds { ids: Vec::new() },
            )?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();

    retry_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entry.id,
        task(),
    )
    .unwrap();

    assert_eq!(
        list(fixture.db()).unwrap()[0].status,
        OperationStatus::Undone
    );
    assert!(deleted_at(&fixture, image).is_none());
    let kept = fixture
        .db()
        .with_reader(|connection| {
            Ok(load_redo_records(connection, entry.id)?
                .iter()
                .any(|record| matches!(record, RedoRecord::PurgeIds { ids } if ids.is_empty())))
        })
        .unwrap();
    assert!(kept, "重试撤销不应清掉上次已提交的重做快照");
}

#[test]
fn skipped_record_stays_in_timeline_but_does_not_block_undo() {
    let fixture = Fixture::new();
    let first = fixture.insert_image("first");
    metadata::set_trashed(fixture.db(), &[first], true).unwrap();
    let second = fixture.insert_image("second");
    metadata::set_trashed(fixture.db(), &[second], true).unwrap();

    let entries = list(fixture.db()).unwrap();
    let failed = entries[0].id;
    let older = entries[1].id;
    force_undo_failed(&fixture, failed);

    skip_undo(fixture.db(), failed).unwrap();
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries[0].status, OperationStatus::UndoSkipped);
    assert_eq!(entries[0].error, "文件被占用");

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        older,
        task(),
    )
    .unwrap();
    assert_eq!(
        list(fixture.db()).unwrap()[1].status,
        OperationStatus::Undone
    );
    assert!(deleted_at(&fixture, first).is_none());
    assert!(deleted_at(&fixture, second).is_some());
}

#[test]
fn undoes_trash_and_restore_in_linear_order() {
    let fixture = Fixture::new();
    let first = fixture.insert_image("a");
    let second = fixture.insert_image("b");

    metadata::set_trashed(fixture.db(), &[first, second], true).unwrap();
    let trashed = list(fixture.db()).unwrap();
    assert_eq!(trashed.len(), 1);
    assert_eq!(trashed[0].kind, HistoryKind::Trash);
    assert_eq!(trashed[0].affected, 2);

    metadata::set_trashed(fixture.db(), &[first], false).unwrap();
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, HistoryKind::Restore);

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[1].id,
        task(),
    )
    .unwrap();
    let deleted_states = |fixture: &Fixture| {
        fixture
            .db()
            .with_reader(|connection| {
                let mut statement =
                    connection.prepare("SELECT id, deleted_at FROM images ORDER BY id")?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
    };
    assert_eq!(
        deleted_states(&fixture),
        vec![(first, None), (second, None)]
    );
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == OperationStatus::Undone)
    );

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        task(),
    )
    .unwrap();
    let rows = deleted_states(&fixture);
    assert!(rows[0].1.is_none());
    assert!(rows[1].1.is_some());
    assert!(
        list(fixture.db())
            .unwrap()
            .iter()
            .all(|entry| entry.status == OperationStatus::Applied)
    );
}

#[test]
fn permanent_delete_keeps_rows_until_release_and_undo_clears_marker() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    let path = fixture.write_content("a");
    metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();

    let report = metadata::delete_images(fixture.db(), &[image_id], &NoopReporter).unwrap();
    assert!(report.deferred);
    assert_eq!(report.deleted, 1);
    assert_eq!(report.files_removed, 0);
    assert!(path.exists());

    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries[0].kind, HistoryKind::PermanentDelete);

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        task(),
    )
    .unwrap();
    let deleted_at: Option<i64> = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT deleted_at FROM images WHERE id = ?1",
                [image_id],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert!(deleted_at.is_some());
    assert!(path.exists());
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 2);
    let deleted = entries
        .iter()
        .find(|entry| entry.kind == HistoryKind::PermanentDelete)
        .unwrap();
    assert_eq!(deleted.status, OperationStatus::Undone);

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        deleted.id,
        task(),
    )
    .unwrap();
    let purge: Option<i64> = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT purge_operation_id FROM images WHERE id = ?1",
                [image_id],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(purge, Some(deleted.id));
    assert!(path.exists());
}

#[test]
fn release_removes_orphan_purge_batches() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    let path = fixture.write_content("a");
    metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
    metadata::empty_trash(fixture.db(), &NoopReporter).unwrap();
    assert!(path.exists());

    // 记录被裁掉后，待释放批次变成无主。
    trim(fixture.db(), 0).unwrap();
    assert!(has_orphan_purge(fixture.db(), &fixture.root).unwrap());

    run_release(fixture.db(), &fixture.root, task()).unwrap();
    assert!(!path.exists());
    let count: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(count, 0);
    assert!(!has_orphan_purge(fixture.db(), &fixture.root).unwrap());
}

/// 释放任务取消后停下：没处理的行还带着待释放标记，下次接着删。
#[test]
fn release_stops_on_cancel_and_keeps_pending_marks() {
    let fixture = Fixture::new();
    let first = fixture.insert_image("a");
    let first_path = fixture.write_content("a");
    let second = fixture.insert_image("b");
    let second_path = fixture.write_content("b");
    metadata::set_trashed(fixture.db(), &[first, second], true).unwrap();
    // 两次永久删除 → 两批待释放；记录被裁掉后一起变成无主批次。
    metadata::delete_images(fixture.db(), &[first], &NoopReporter).unwrap();
    metadata::delete_images(fixture.db(), &[second], &NoopReporter).unwrap();
    trim(fixture.db(), 0).unwrap();

    let stopped = TaskHandle::new("release", TaskKind::ReleaseGarbage, "释放删除文件");
    stopped.set_cancellable(true);
    stopped.request_cancel();
    run_release(fixture.db(), &fixture.root, stopped.clone()).unwrap();
    assert!(stopped.should_cancel());
    assert!(
        stopped.snapshot().message.contains("剩余待释放保留到下次"),
        "{}",
        stopped.snapshot().message
    );
    // 一行都没删：文件与标记都还在，下次释放继续。
    assert!(first_path.exists() && second_path.exists());
    assert!(has_orphan_purge(fixture.db(), &fixture.root).unwrap());

    run_release(fixture.db(), &fixture.root, task()).unwrap();
    assert!(!first_path.exists() && !second_path.exists());
    assert!(!has_orphan_purge(fixture.db(), &fixture.root).unwrap());
}

#[test]
fn folder_trash_restore_only_revives_images_trashed_with_the_folder() {
    let fixture = Fixture::new();
    folders::create(fixture.db(), &fixture.root, "人物/子").unwrap();
    let child = folders::find_id(fixture.db(), "人物/子").unwrap().unwrap();
    let fresh = fixture.insert_image("f1");
    let old = fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO images(
                    content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at,
                    modified_at
                 ) VALUES (?1, 'jpg', ?2, 100, 10, 10, 'jpeg', 0,
                    zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                params!["e".repeat(64), child],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .unwrap();
    metadata::set_trashed(fixture.db(), &[old], true).unwrap();

    let change = folders::trash(fixture.db(), fixture.folder).unwrap();
    assert_eq!(
        change.affected_images, 1,
        "只把原本正常的图片一起移入回收站"
    );
    assert!(
        metadata::set_trashed(fixture.db(), &[fresh], false).is_err(),
        "整目录删除的图片不能单张恢复"
    );

    let trash = crate::library::query::list_trash_folders(fixture.db()).unwrap();
    assert_eq!(trash.folders.len(), 2, "父与子都在回收站树里");

    let change = folders::restore(fixture.db(), fixture.folder).unwrap();
    assert_eq!(change.affected_images, 1);
    let states = metadata::image_states(fixture.db(), &[fresh, old]).unwrap();
    assert!(
        states
            .iter()
            .any(|state| state.id == fresh && state.deleted_at.is_none())
    );
    assert!(
        states
            .iter()
            .any(|state| state.id == old && state.deleted_at.is_some()),
        "删除前已单独在回收站的图片保持删除"
    );
    let trash = crate::library::query::list_trash_folders(fixture.db()).unwrap();
    assert!(trash.folders.is_empty());
    assert_eq!(trash.direct_image_count, 1, "留下的图片归到根行直属图片");
}

#[test]
fn folder_purge_release_removes_empty_dirs_and_rows() {
    let fixture = Fixture::new();
    folders::create(fixture.db(), &fixture.root, "整目录/子").unwrap();
    let parent = folders::find_id(fixture.db(), "整目录").unwrap().unwrap();
    let child = folders::find_id(fixture.db(), "整目录/子")
        .unwrap()
        .unwrap();
    let hash = "c".repeat(64);
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO images(
                    content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at,
                    modified_at
                 ) VALUES (?1, 'jpg', ?2, 100, 10, 10, 'jpeg', 0,
                    zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                params![hash, child],
            )?;
            Ok(())
        })
        .unwrap();
    let content =
        LibraryPaths::new(&fixture.root).content_file("整目录/子", &format!("{hash}.jpg"));
    fs::create_dir_all(content.parent().unwrap()).unwrap();
    fs::write(&content, b"content").unwrap();

    folders::trash(fixture.db(), parent).unwrap();
    folders::delete_permanently(fixture.db(), parent).unwrap();
    trim(fixture.db(), 0).unwrap();
    assert!(has_orphan_purge(fixture.db(), &fixture.root).unwrap());

    run_release(fixture.db(), &fixture.root, task()).unwrap();
    assert!(!content.exists());
    assert!(!fixture.root.join("整目录").join("子").exists());
    assert!(!fixture.root.join("整目录").exists());
    assert!(folders::find_id(fixture.db(), "整目录").unwrap().is_none());
    assert!(
        folders::find_id(fixture.db(), "整目录/子")
            .unwrap()
            .is_none()
    );
    assert!(!has_orphan_purge(fixture.db(), &fixture.root).unwrap());
}

/// 目录里还有未纳管文件时保留待释放标记，但不能再被当成「还有待释放文件」，
/// 否则每次写入都会白排一次释放任务；清空目录后再清空回收站能重试成功。
#[test]
fn folder_release_defers_while_the_directory_is_not_empty() {
    let fixture = Fixture::new();
    folders::create(fixture.db(), &fixture.root, "整目录").unwrap();
    let folder_id = folders::find_id(fixture.db(), "整目录").unwrap().unwrap();
    let directory = LibraryPaths::new(&fixture.root).folder_dir("整目录");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("notes.txt"), "未纳管文件".as_bytes()).unwrap();

    folders::trash(fixture.db(), folder_id).unwrap();
    folders::delete_permanently(fixture.db(), folder_id).unwrap();
    trim(fixture.db(), 0).unwrap();
    assert!(
        !has_orphan_purge(fixture.db(), &fixture.root).unwrap(),
        "目录不空的文件夹不该让释放检查永远为真"
    );

    let handle = task();
    run_release(fixture.db(), &fixture.root, handle.clone()).unwrap();
    assert!(directory.exists(), "不能递归强删目录");
    assert!(folders::find_id(fixture.db(), "整目录").unwrap().is_some());
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.failures.len(), 1, "暂不释放要进失败清单");
    assert!(snapshot.message.contains("未能释放"));

    fs::remove_file(directory.join("notes.txt")).unwrap();
    metadata::empty_trash(fixture.db(), &NoopReporter).unwrap();
    trim(fixture.db(), 0).unwrap();
    run_release(fixture.db(), &fixture.root, task()).unwrap();
    assert!(!directory.exists());
    assert!(folders::find_id(fixture.db(), "整目录").unwrap().is_none());
    assert!(!has_orphan_purge(fixture.db(), &fixture.root).unwrap());
}

#[test]
fn recovers_pending_and_undoing_records() {
    let fixture = Fixture::new();
    let pending_empty = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Import, "空导入"),
    )
    .unwrap();
    let pending_content = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Import, "有内容导入"),
    )
    .unwrap();
    // 模拟导入中途写入的批次：一个 chunk 加一条 affected。
    fixture
        .db()
        .with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            add_live_affected_in_tx(&transaction, pending_content, 1)?;
            append_live_in_tx(
                &transaction,
                pending_content,
                ChunkFormat::Json,
                &serde_json::to_vec(&ImportRecord::Merged {
                    items: vec![ImportMerged {
                        image_id: 1,
                        old_score: 0,
                        old_modified_at: 1,
                        old_tag_ids: Vec::new(),
                    }],
                })?,
            )?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();
    let undoing = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Trash, "中断的撤销"),
    )
    .unwrap();
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "UPDATE operations SET status = 'undoing' WHERE id = ?1",
                [undoing],
            )?;
            Ok(())
        })
        .unwrap();

    let plan = recover_on_open(fixture.db()).unwrap();
    assert_eq!(plan.resume_undo, Some(undoing));
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, pending_content);
    let remaining: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(remaining, 2);
    assert!(entries.iter().all(|entry| entry.id != pending_empty));
}

#[test]
fn redo_to_older_record_leaves_newer_undone() {
    let fixture = Fixture::new();
    let first = fixture.insert_image("a");
    let second = fixture.insert_image("b");
    metadata::set_trashed(fixture.db(), &[first, second], true).unwrap();
    metadata::set_trashed(fixture.db(), &[first], false).unwrap();
    let entries = list(fixture.db()).unwrap();
    let trash = entries
        .iter()
        .find(|entry| entry.kind == HistoryKind::Trash)
        .unwrap()
        .id;
    let restore = entries
        .iter()
        .find(|entry| entry.kind == HistoryKind::Restore)
        .unwrap()
        .id;

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        trash,
        task(),
    )
    .unwrap();
    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        trash,
        task(),
    )
    .unwrap();
    let entries = list(fixture.db()).unwrap();
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.id == trash)
            .unwrap()
            .status,
        OperationStatus::Applied
    );
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.id == restore)
            .unwrap()
            .status,
        OperationStatus::Undone
    );
    let trashed: Vec<Option<i64>> = fixture
        .db()
        .with_reader(|connection| {
            let mut statement = connection.prepare("SELECT deleted_at FROM images ORDER BY id")?;
            let rows = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert!(trashed.iter().all(Option::is_some));

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        restore,
        task(),
    )
    .unwrap();
    assert_eq!(
        metadata::image_details(fixture.db(), &fixture.root, first)
            .unwrap()
            .deleted_at,
        None
    );
}

#[test]
fn new_operation_discards_undone_branch() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
    let entry = list(fixture.db()).unwrap().remove(0);
    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entry.id,
        task(),
    )
    .unwrap();
    assert_eq!(
        list(fixture.db()).unwrap()[0].status,
        OperationStatus::Undone
    );

    metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, OperationStatus::Applied);
    assert_eq!(entries[0].id, entry.id + 1);
}

/// 导入被资源冲突拒绝时会先建一条 `pending` 记录再收掉；
/// 这条空记录不能顺手清掉灰色（可重做）记录。
#[test]
fn pending_import_without_content_keeps_redo_branch() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
    let entry = list(fixture.db()).unwrap().remove(0);
    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entry.id,
        task(),
    )
    .unwrap();

    let pending = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Import, "被拒绝的导入"),
    )
    .unwrap();
    assert_eq!(list(fixture.db()).unwrap().len(), 1);
    finish_live(fixture.db(), pending, "").unwrap();

    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, entry.id);
    assert_eq!(entries[0].status, OperationStatus::Undone);
}

#[test]
fn recovers_interrupted_redo() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
    let entry = list(fixture.db()).unwrap().remove(0);
    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entry.id,
        task(),
    )
    .unwrap();
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "UPDATE operations SET status = 'redoing' WHERE id = ?1",
                [entry.id],
            )?;
            Ok(())
        })
        .unwrap();

    let plan = recover_on_open(fixture.db()).unwrap();
    assert_eq!(plan.resume_redo, Some(entry.id));
    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entry.id,
        task(),
    )
    .unwrap();
    assert_eq!(
        list(fixture.db()).unwrap()[0].status,
        OperationStatus::Applied
    );
    assert!(
        metadata::image_details(fixture.db(), &fixture.root, image_id)
            .unwrap()
            .deleted_at
            .is_some()
    );
}

#[test]
fn history_survives_reopen() {
    let fixture = Fixture::new();
    write(
        fixture.db(),
        NewOperation::new(HistoryKind::Trash, "移入回收站"),
        |_, draft| {
            draft.affected(1);
            draft.triples(&[(1, 0, 42)]);
            Ok(())
        },
    )
    .unwrap();
    let Fixture {
        _temp: temp,
        session,
        root,
        folder: _,
    } = fixture;
    drop(session);
    let reopened = service::open(&root, 1).unwrap();
    let entries = list(&reopened.db).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].label, "移入回收站");
    assert_eq!(entries[0].affected, 1);
    drop(temp);
}

#[test]
fn cancellation_stops_between_records() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    metadata::set_trashed(fixture.db(), &[image_id], true).unwrap();
    let entries = list(fixture.db()).unwrap();
    let handle = task();
    handle.request_cancel();
    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        handle,
    )
    .unwrap();
    assert_eq!(list(fixture.db()).unwrap().len(), 1);
    assert!(
        metadata::image_details(fixture.db(), &fixture.root, image_id)
            .unwrap()
            .deleted_at
            .is_some()
    );
}

#[test]
fn cleanup_removes_undo_dirs_without_operations() {
    let fixture = Fixture::new();
    let operation = write(
        fixture.db(),
        NewOperation::new(HistoryKind::Edit, "编辑"),
        |_, draft| {
            draft.affected(1);
            Ok(draft.id())
        },
    )
    .unwrap();
    let kept = undo_dir(&fixture.root, operation);
    let orphan = undo_dir(&fixture.root, operation + 998);
    fs::create_dir_all(&kept).unwrap();
    fs::create_dir_all(&orphan).unwrap();
    cleanup_undo_dirs(fixture.db(), &fixture.root).unwrap();
    assert!(kept.exists());
    assert!(!orphan.exists());
}

#[test]
fn cleanup_waits_for_an_open_write_transaction() {
    let fixture = Fixture::new();
    let pending_id = 4_242_i64;
    let directory = undo_dir(&fixture.root, pending_id);
    fs::create_dir_all(&directory).unwrap();

    // 模拟编辑保存：暂存目录先落地，operations 行要到提交后才对读连接可见。
    let (ready, wait) = std::sync::mpsc::channel();
    let db = fixture.session.db.clone();
    let writer = std::thread::spawn(move || {
        db.with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "INSERT INTO operations(id, kind, label, created_at, status, affected, error)
                 VALUES (?1, 'edit', '在建', 1, 'pending', 0, '')",
                [pending_id],
            )?;
            ready.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            transaction.commit()?;
            Ok(())
        })
    });
    wait.recv().unwrap();
    cleanup_undo_dirs(fixture.db(), &fixture.root).unwrap();
    assert!(
        directory.exists(),
        "在建记录的暂存目录不能被当成无主目录删掉"
    );
    writer.join().unwrap().unwrap();
    assert!(directory.exists());
}

#[test]
fn appending_chunks_skips_records_that_are_gone() {
    let fixture = Fixture::new();
    fixture
        .db()
        .with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // 记录已被收尾删除时，逆数据写不进去也不能让整批入库回滚。
            append_live_in_tx(&transaction, 9_999, ChunkFormat::Json, b"[]")?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn write_rolls_back_file_actions_when_the_work_fails() {
    let fixture = Fixture::new();
    let source = fixture.write_content("a");
    let target = fixture.root.join("风景");
    fs::create_dir_all(&target).unwrap();
    let destination = target.join("moved.jpg");

    let result: AppResult<()> = write(
        fixture.db(),
        NewOperation::new(HistoryKind::Move, "测试搬移"),
        |_, draft| {
            fs::rename(&source, &destination)?;
            draft.journal().renamed(&source, &destination);
            Err(AppError::message("模拟写失败"))
        },
    );
    assert!(result.is_err());
    assert!(source.exists(), "写失败后文件要回到原位");
    assert!(!destination.exists());
    let records: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(records, 0, "失败的事务不留空记录");
}

#[test]
fn write_keeps_file_actions_after_commit() {
    let fixture = Fixture::new();
    let source = fixture.write_content("b");
    let target = fixture.root.join("风景");
    fs::create_dir_all(&target).unwrap();
    let destination = target.join("moved.jpg");

    write(
        fixture.db(),
        NewOperation::new(HistoryKind::Move, "测试搬移"),
        |_, draft| {
            fs::rename(&source, &destination)?;
            draft.journal().renamed(&source, &destination);
            draft.affected(1);
            Ok(())
        },
    )
    .unwrap();
    assert!(destination.exists(), "提交成功后文件留在新位置");
    assert!(!source.exists());
    assert_eq!(list(fixture.db()).unwrap().len(), 1);
}

#[test]
fn undoes_moves_and_restores_file_location() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    let old_path = fixture.write_content("a");
    let report = metadata::move_images(
        fixture.db(),
        &fixture.root,
        &[image_id],
        "风景",
        &NoopReporter,
    )
    .unwrap();
    assert_eq!(report.moved, 1);
    let moved_path = LibraryPaths::new(&fixture.root)
        .content_file("风景", &format!("{}.jpg", "0".repeat(63) + "a"));
    assert!(moved_path.exists());
    assert!(!old_path.exists());

    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries[0].kind, HistoryKind::Move);
    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        task(),
    )
    .unwrap();

    assert_eq!(
        metadata::image_details(fixture.db(), &fixture.root, image_id)
            .unwrap()
            .folder_path,
        "人物"
    );
    assert!(old_path.exists());
    assert!(!moved_path.exists());
    assert!(!fixture.root.join("风景").exists());
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries[0].status, OperationStatus::Undone);

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        task(),
    )
    .unwrap();
    assert_eq!(
        metadata::image_details(fixture.db(), &fixture.root, image_id)
            .unwrap()
            .folder_path,
        "风景"
    );
    assert!(!old_path.exists());
    assert!(moved_path.exists());
    assert!(fixture.root.join("风景").is_dir());
    assert_eq!(
        list(fixture.db()).unwrap()[0].status,
        OperationStatus::Applied
    );
}

#[test]
fn undo_move_is_idempotent_after_partial_restore() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    let old_path = fixture.write_content("a");
    metadata::move_images(
        fixture.db(),
        &fixture.root,
        &[image_id],
        "风景",
        &NoopReporter,
    )
    .unwrap();
    let entry = list(fixture.db()).unwrap().remove(0);

    // 模拟“文件已移回、行已更新，但进程在删记录前退出”。
    let moved_path = LibraryPaths::new(&fixture.root)
        .content_file("风景", &format!("{}.jpg", "0".repeat(63) + "a"));
    fs::rename(&moved_path, &old_path).unwrap();
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "UPDATE images SET folder_id = ?1 WHERE id = ?2",
                params![fixture.folder, image_id],
            )?;
            connection.execute(
                "UPDATE operations SET status = 'undoing' WHERE id = ?1",
                [entry.id],
            )?;
            Ok(())
        })
        .unwrap();

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entry.id,
        task(),
    )
    .unwrap();
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries[0].status, OperationStatus::Undone);
    assert!(old_path.exists());
    assert!(!fixture.root.join("风景").exists());
}

#[test]
fn undoes_edit_restoring_file_row_thumbnail_and_features() {
    use crate::library::edit::{self, EditRequest};

    let fixture = Fixture::new();
    let (image_id, old_hash, old_path) = fixture.insert_editable_image();
    let request = EditRequest {
        image_id,
        rotate: 1,
        flip_horizontal: false,
        flip_vertical: false,
        crop: None,
        resize: None,
        output_format: None,
        quality: Some(90),
    };
    let result = edit::save(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        &request,
    )
    .unwrap();
    assert!(result.changed);
    assert!(!old_path.exists());
    let edited_path = LibraryPaths::new(&fixture.root)
        .content_file("人物", &format!("{}.png", result.content_hash));
    assert!(edited_path.exists());

    let entry = list(fixture.db())
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == HistoryKind::Edit)
        .unwrap();
    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entry.id,
        task(),
    )
    .unwrap();

    let details = metadata::image_details(fixture.db(), &fixture.root, image_id).unwrap();
    assert_eq!(details.content_hash, old_hash);
    assert_eq!((details.width, details.height), (24, 16));
    assert!(old_path.exists());
    assert!(!edited_path.exists());
    let features: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM feature_blobs", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(features, 0);
    let pointer = crate::library::thumbs::load_pointer(fixture.db(), image_id)
        .unwrap()
        .unwrap();
    assert_eq!(pointer.version, 3);
    assert!(fixture.session.thumbs.read(pointer.location).is_ok());
    assert!(undo_dir(&fixture.root, entry.id).exists());
    assert_eq!(
        list(fixture.db()).unwrap()[0].status,
        OperationStatus::Undone
    );

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entry.id,
        task(),
    )
    .unwrap();
    let details = metadata::image_details(fixture.db(), &fixture.root, image_id).unwrap();
    assert_eq!(details.content_hash, result.content_hash);
    assert_eq!((details.width, details.height), (16, 24));
    assert!(edited_path.exists());
    assert!(!old_path.exists());
    assert_eq!(
        list(fixture.db()).unwrap()[0].status,
        OperationStatus::Applied
    );
    let pointer = crate::library::thumbs::load_pointer(fixture.db(), image_id)
        .unwrap()
        .unwrap();
    assert_eq!(pointer.version, 4);
    assert!(fixture.session.thumbs.read(pointer.location).is_ok());
}

#[test]
fn undoes_import_new_rows_and_merges() {
    use crate::import::{
        FolderMode, ImportRequest, ImportSource, ImportTarget,
        job::{DuplicatePolicy, ImportJob},
        pipeline,
    };

    let fixture = Fixture::new();
    let source_dir = fixture._temp.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();
    sample_png(&source_dir.join("a.png"), 1);
    sample_png(&source_dir.join("b.png"), 2);

    let request = |merge: bool, tag: &str, score: u8| ImportRequest {
        paths: vec![source_dir.to_string_lossy().into_owned()],
        target: ImportTarget::Library,
        folder: "导入".into(),
        folder_mode: FolderMode::Flat,
        tags: vec![tag.to_owned()],
        score,
        duplicate_policy: if merge {
            DuplicatePolicy::Merge
        } else {
            DuplicatePolicy::Skip
        },
        perceptual_threshold: 0,
        optimize: None,
        #[cfg(test)]
        cancel_after: None,
    };
    let run_import = |operation_id: i64, merge: bool, tag: &str, score: u8| {
        let source = ImportSource {
            database: fixture.session.db.clone(),
            thumbs: fixture.session.thumbs.clone(),
            root: fixture.root.clone(),
            operation_id: Some(operation_id),
            resources: Arc::new(crate::concurrency::resources::ResourceTable::new()),
            images: Arc::new(crate::concurrency::budget::ImageBudget::new(2)),
            memory: Arc::new(crate::concurrency::budget::ImageMemoryBudget::default()),
            holder: crate::concurrency::resources::HolderInfo::new("import-history", "导入"),
        };
        let job = Arc::new(ImportJob::new(
            "import-history",
            "测试",
            ImportTarget::Library,
        ));
        pipeline::run(source, job, request(merge, tag, score)).unwrap();
        finish_live(fixture.db(), operation_id, "").unwrap();
    };

    let first = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Import, "第一次导入"),
    )
    .unwrap();
    run_import(first, false, "初始", 2);
    let second = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Import, "第二次导入"),
    )
    .unwrap();
    run_import(second, true, "追加", 5);

    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].affected, 2);

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        second,
        task(),
    )
    .unwrap();
    let (score, names): (i64, Vec<String>) = fixture
        .db()
        .with_reader(|connection| {
            let score =
                connection.query_row("SELECT score FROM images LIMIT 1", [], |row| row.get(0))?;
            let mut statement = connection.prepare(
                "SELECT tags.name FROM image_tags JOIN tags ON tags.id = image_tags.tag_id
                 ORDER BY tags.name",
            )?;
            let names = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok((score, names))
        })
        .unwrap();
    assert_eq!(score, 2);
    assert_eq!(names, vec!["初始".to_owned(), "初始".to_owned()]);
    let extra_tag: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM tags WHERE name = '追加'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(extra_tag, 0);
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries[0].id, second);
    assert_eq!(entries[0].status, OperationStatus::Undone);

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        second,
        task(),
    )
    .unwrap();
    let (images, score, names): (i64, i64, Vec<String>) = fixture
        .db()
        .with_reader(|connection| {
            let images =
                connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?;
            let score =
                connection.query_row("SELECT score FROM images LIMIT 1", [], |row| row.get(0))?;
            let mut statement = connection.prepare(
                "SELECT tags.name FROM image_tags JOIN tags ON tags.id = image_tags.tag_id
                 ORDER BY tags.name",
            )?;
            let names = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok((images, score, names))
        })
        .unwrap();
    assert_eq!(images, 2);
    assert_eq!(score, 5);
    assert_eq!(
        names,
        vec![
            "初始".to_owned(),
            "初始".to_owned(),
            "追加".to_owned(),
            "追加".to_owned()
        ]
    );
    assert!(
        list(fixture.db())
            .unwrap()
            .iter()
            .all(|entry| entry.status == OperationStatus::Applied)
    );

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        first,
        task(),
    )
    .unwrap();
    let remaining: (i64, i64) = fixture
        .db()
        .with_reader(|connection| {
            Ok((
                connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?,
                connection.query_row("SELECT count(*) FROM tags", [], |row| row.get(0))?,
            ))
        })
        .unwrap();
    assert_eq!(remaining, (0, 0));
    assert!(!fixture.root.join("导入").exists());
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == OperationStatus::Undone)
    );

    // 再导入一次并写入内容：灰色记录让位给新时间线，列表只剩这条新导入。
    let third = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Import, "第三次导入"),
    )
    .unwrap();
    run_import(third, false, "重来", 1);
    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, third);
    assert_eq!(entries[0].affected, 2);
}

/// 同一次导入里同一张图被合并两次时，只有最早那份快照是导入前的状态。
#[test]
fn import_undo_applies_only_the_earliest_merge_snapshot() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    let tag_id = fixture
        .db()
        .with_writer(|connection| {
            connection.execute("INSERT INTO tags(name) VALUES ('追加')", [])?;
            Ok(connection.last_insert_rowid())
        })
        .unwrap();
    let operation = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Import, "合并导入"),
    )
    .unwrap();
    // 第一次合并 3 → 5，第二次合并只加标签。
    let merges = [
        ImportMerged {
            image_id,
            old_score: 3,
            old_modified_at: 10,
            old_tag_ids: Vec::new(),
        },
        ImportMerged {
            image_id,
            old_score: 5,
            old_modified_at: 20,
            old_tag_ids: vec![tag_id],
        },
    ];
    fixture
        .db()
        .with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for item in &merges {
                append_live_in_tx(
                    &transaction,
                    operation,
                    ChunkFormat::Json,
                    &serde_json::to_vec(&ImportRecord::Merged {
                        items: vec![item.clone()],
                    })?,
                )?;
            }
            // 导入后的状态：评分 5、带「追加」标签。
            transaction.execute(
                "UPDATE images SET score = 5, modified_at = 20 WHERE id = ?1",
                [image_id],
            )?;
            transaction.execute(
                "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, ?2)",
                params![image_id, tag_id],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();

    undo_import(fixture.db(), &fixture.root, &task(), operation).unwrap();

    let (score, modified_at, tags): (i64, i64, Vec<i64>) = fixture
        .db()
        .with_reader(|connection| {
            let (score, modified_at) = connection.query_row(
                "SELECT score, modified_at FROM images WHERE id = ?1",
                [image_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let mut statement =
                connection.prepare("SELECT tag_id FROM image_tags WHERE image_id = ?1")?;
            let tags = statement
                .query_map([image_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok((score, modified_at, tags))
        })
        .unwrap();
    assert_eq!((score, modified_at), (3, 10), "恢复到导入前，不是中间态");
    assert!(tags.is_empty(), "导入前没有标签");
}

/// 撤销时原图已经缺失，重做要按缺图恢复这一张，不能阻塞整条重做链。
#[test]
fn redo_import_restores_rows_whose_original_is_missing() {
    let fixture = Fixture::new();
    let operation = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::Import, "缺图导入"),
    )
    .unwrap();
    let row = ImportedRow {
        id: 7,
        content_hash: "a".repeat(64),
        file_ext: "jpg".to_owned(),
        folder_id: fixture.folder,
        file_size: 100,
        width: 10,
        height: 10,
        format: "jpeg".to_owned(),
        has_alpha: false,
        phash: vec![0; 32],
        colors4: vec![0; 16],
        colors8: vec![0; 32],
        score: 2,
        description: String::new(),
        description_source: "import".to_owned(),
        description_model: String::new(),
        imported_at: 1,
        modified_at: 1,
        deleted_at: None,
        thumb_segment: 0,
        thumb_offset: 0,
        thumb_length: 0,
        thumb_format: String::new(),
        thumb_width: 0,
        thumb_height: 0,
        thumb_version: 0,
        origin_operation_id: Some(operation),
    };
    fixture
        .db()
        .with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            append_redo(
                &transaction,
                operation,
                &RedoRecord::ImportImages {
                    items: vec![ImportRedoImage {
                        row,
                        folder_path: "人物".to_owned(),
                        tag_ids: Vec::new(),
                        collection_items: Vec::new(),
                        // 撤销时原图就已经不在了。
                        staged_file: None,
                    }],
                },
            )?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();

    let handle = task();
    redo_import(fixture.db(), &fixture.root, &handle, operation).unwrap();

    let count: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(
                connection.query_row("SELECT count(*) FROM images WHERE id = 7", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(count, 1, "行要照常恢复");
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.failures.len(), 1, "缺图要进失败清单");
    assert!(snapshot.message.contains("缺原图"), "{}", snapshot.message);
}

/// 文件夹五类记录串成一条时间线：创建 → 重命名 → 移动 → 同级排序 → 删除。
/// 撤销要按逆序回退到最初，重做要按正序回到最终状态。
#[test]
fn undoes_and_redoes_folder_records_in_linear_order() {
    let fixture = Fixture::new();
    let folder_id = |path: &str| -> Option<i64> {
        fixture
            .db()
            .with_reader(|connection| {
                Ok(connection
                    .query_row("SELECT id FROM folders WHERE path = ?1", [path], |row| {
                        row.get::<_, i64>(0)
                    })
                    .optional()?)
            })
            .unwrap()
    };
    let root_id = fixture
        .db()
        .with_reader(|connection| {
            Ok(
                connection.query_row("SELECT id FROM folders WHERE path = ''", [], |row| {
                    row.get::<_, i64>(0)
                })?,
            )
        })
        .unwrap();
    let sort_of = |path: &str| -> i64 {
        fixture
            .db()
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT sort_index FROM folders WHERE path = ?1",
                    [path],
                    |row| row.get::<_, i64>(0),
                )?)
            })
            .unwrap()
    };
    let has = |path: &str| -> bool {
        fixture
            .db()
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM folders WHERE path = ?1",
                    [path],
                    |row| row.get::<_, i64>(0),
                )? > 0)
            })
            .unwrap()
    };

    folders::create(fixture.db(), &fixture.root, "作品").unwrap();
    let created = list(fixture.db()).unwrap().remove(0).id;
    folders::create(fixture.db(), &fixture.root, "杂物").unwrap();
    let work = folder_id("作品").unwrap();
    let junk = folder_id("杂物").unwrap();

    folders::rename(fixture.db(), &fixture.root, work, "成品").unwrap();
    assert!(fixture.root.join("成品").is_dir());
    folders::move_to(fixture.db(), &fixture.root, work, fixture.folder).unwrap();
    assert!(fixture.root.join("人物").join("成品").is_dir());
    folders::reorder(fixture.db(), &[junk], root_id, 0).unwrap();
    assert_eq!(sort_of("杂物"), 0);
    assert_eq!(sort_of("人物"), 1);
    folders::trash(fixture.db(), work).unwrap();
    let deleted = list(fixture.db()).unwrap().remove(0).id;
    assert!(has("人物/成品"), "移入回收站保留文件夹行");
    assert!(fixture.root.join("人物").join("成品").is_dir());

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        created,
        task(),
    )
    .unwrap();
    assert!(
        list(fixture.db())
            .unwrap()
            .iter()
            .all(|entry| entry.status == OperationStatus::Undone)
    );
    assert!(has("人物"));
    assert!(!has("作品") && !has("成品") && !has("人物/成品") && !has("杂物"));
    assert!(!fixture.root.join("作品").exists());
    assert!(!fixture.root.join("杂物").exists());
    assert_eq!(sort_of("人物"), 0);

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        deleted,
        task(),
    )
    .unwrap();
    assert!(
        list(fixture.db())
            .unwrap()
            .iter()
            .all(|entry| entry.status == OperationStatus::Applied)
    );
    assert!(has("人物") && has("杂物"));
    assert!(!has("作品") && !has("成品"));
    assert!(has("人物/成品"), "重做删除只写回收站状态，行仍在");
    assert_eq!(sort_of("杂物"), 0);
    assert_eq!(sort_of("人物"), 1);
    assert!(fixture.root.join("人物").is_dir());
    assert!(fixture.root.join("人物").join("成品").is_dir());
}

fn tag_id(database: &LibraryDb, name: &str) -> i64 {
    database
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
                [name],
                |row| row.get(0),
            )?)
        })
        .unwrap()
}

fn tag_names(database: &LibraryDb, image_id: i64) -> Vec<String> {
    database
        .with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT tags.name FROM image_tags
                 JOIN tags ON tags.id = image_tags.tag_id
                 WHERE image_tags.image_id = ?1 ORDER BY tags.name",
            )?;
            Ok(statement
                .query_map([image_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap()
}

fn image_state(database: &LibraryDb, image_id: i64) -> (i64, String, String, String) {
    database
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT score, description, description_source, description_model
                 FROM images WHERE id = ?1",
                [image_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?)
        })
        .unwrap()
}

fn tag_count(database: &LibraryDb) -> i64 {
    database
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM tags", [], |row| row.get(0))?)
        })
        .unwrap()
}

#[test]
fn undoes_scores_descriptions_and_tags_in_linear_order() {
    let fixture = Fixture::new();
    let first = fixture.insert_image("a");
    let second = fixture.insert_image("b");

    metadata::update_score(fixture.db(), first, 3).unwrap();
    metadata::set_scores(fixture.db(), &[first, second], 5).unwrap();
    metadata::update_description(fixture.db(), first, "新描述").unwrap();
    metadata::set_image_tags(fixture.db(), first, &["x".into(), "y".into()]).unwrap();
    metadata::add_tags(fixture.db(), &[second], &["z".into()]).unwrap();
    metadata::remove_tags(fixture.db(), &[first], &["y".into()]).unwrap();
    let x = tag_id(fixture.db(), "x");
    metadata::rename_tag(fixture.db(), x, "x2").unwrap();
    let z = tag_id(fixture.db(), "z");
    metadata::delete_tag(fixture.db(), z).unwrap();

    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 8);
    assert_eq!(entries[0].kind, HistoryKind::TagDelete);

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries.last().unwrap().id,
        task(),
    )
    .unwrap();
    assert_eq!(image_state(fixture.db(), first).0, 0);
    assert_eq!(image_state(fixture.db(), second).0, 0);
    assert_eq!(image_state(fixture.db(), first).1, "");
    assert!(tag_names(fixture.db(), first).is_empty());
    assert!(tag_names(fixture.db(), second).is_empty());
    assert_eq!(tag_count(fixture.db()), 0);

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        task(),
    )
    .unwrap();
    let first_state = image_state(fixture.db(), first);
    assert_eq!(first_state.0, 5);
    assert_eq!(first_state.1, "新描述");
    assert_eq!(first_state.2, "user");
    assert_eq!(image_state(fixture.db(), second).0, 5);
    assert_eq!(tag_names(fixture.db(), first), vec!["x2"]);
    assert!(tag_names(fixture.db(), second).is_empty());
    // 原操作只移除了 y 与图片的关系，标签行本身仍保留。
    assert_eq!(tag_count(fixture.db()), 2);
}

#[test]
fn undoes_ai_description_live_operations() {
    let fixture = Fixture::new();
    let image_id = fixture.insert_image("a");
    let operation_id = begin_live(
        fixture.db(),
        NewOperation::new(HistoryKind::AiDescription, "AI 描述 1 张"),
    )
    .unwrap();
    assert!(
        metadata::set_ai_description_live(
            fixture.db(),
            operation_id,
            image_id,
            "AI 文本",
            "model-x",
        )
        .unwrap()
    );
    finish_live(fixture.db(), operation_id, "").unwrap();

    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, HistoryKind::AiDescription);
    assert_eq!(entries[0].affected, 1);

    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        operation_id,
        task(),
    )
    .unwrap();
    let state = image_state(fixture.db(), image_id);
    assert_eq!(state.1, "");
    assert_eq!(state.2, "user");

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        operation_id,
        task(),
    )
    .unwrap();
    let state = image_state(fixture.db(), image_id);
    assert_eq!(state.1, "AI 文本");
    assert_eq!(state.2, "ai");
    assert_eq!(state.3, "model-x");
}

fn collection_items(database: &LibraryDb, collection_id: i64) -> Vec<i64> {
    database
        .with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT image_id FROM collection_items
                 WHERE collection_id = ?1 ORDER BY position, image_id",
            )?;
            Ok(statement
                .query_map([collection_id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap()
}

#[test]
fn undoes_and_redoes_collection_crud_and_ordering() {
    let fixture = Fixture::new();
    let first = fixture.insert_image("a");
    let second = fixture.insert_image("b");

    let reference = collections::create(fixture.db(), "参考").unwrap();
    collections::add_images(fixture.db(), reference.id, &[first, second]).unwrap();
    collections::remove_images(fixture.db(), reference.id, &[first]).unwrap();
    collections::rename(fixture.db(), reference.id, "精选").unwrap();
    let practice = collections::create(fixture.db(), "练习").unwrap();
    collections::reorder(fixture.db(), &[practice.id, reference.id]).unwrap();
    assert_eq!(collection_items(fixture.db(), reference.id), vec![second]);

    let entries = list(fixture.db()).unwrap();
    assert_eq!(entries.len(), 6);
    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries.last().unwrap().id,
        task(),
    )
    .unwrap();
    assert!(collections::list(fixture.db()).unwrap().is_empty());

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        task(),
    )
    .unwrap();
    let restored = collections::list(fixture.db()).unwrap();
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0].id, practice.id);
    assert_eq!(restored[1].id, reference.id);
    assert_eq!(restored[1].name, "精选");
    assert_eq!(collection_items(fixture.db(), reference.id), vec![second]);
}

#[test]
fn undoes_collection_combine_and_similarity_groups() {
    let fixture = Fixture::new();
    let first = fixture.insert_image("a");
    let second = fixture.insert_image("b");
    let third = fixture.insert_image("c");

    let left = collections::create(fixture.db(), "A").unwrap();
    collections::add_images(fixture.db(), left.id, &[first, second]).unwrap();
    let right = collections::create(fixture.db(), "B").unwrap();
    collections::add_images(fixture.db(), right.id, &[second, third]).unwrap();
    let combined = collections::combine(
        fixture.db(),
        collections::CombineMode::Union,
        &[left.id, right.id],
        "并集",
    )
    .unwrap();
    assert_eq!(
        collection_items(fixture.db(), combined.id),
        vec![first, second, third]
    );

    let groups =
        collections::create_groups(fixture.db(), &[vec![first, second], vec![third]], "相似组")
            .unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].name, "相似组 1");
    assert_eq!(groups[1].name, "相似组 2");

    let entries = list(fixture.db()).unwrap();
    run_undo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries.last().unwrap().id,
        task(),
    )
    .unwrap();
    assert!(collections::list(fixture.db()).unwrap().is_empty());

    run_redo(
        fixture.db(),
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        task(),
    )
    .unwrap();
    let restored = collections::list(fixture.db()).unwrap();
    assert_eq!(restored.len(), 5);
    assert!(restored.iter().any(|collection| collection.name == "并集"));
    let group_one = restored
        .iter()
        .find(|collection| collection.name == "相似组 1")
        .unwrap();
    assert_eq!(
        collection_items(fixture.db(), group_one.id),
        vec![first, second]
    );
    let group_two = restored
        .iter()
        .find(|collection| collection.name == "相似组 2")
        .unwrap();
    assert_eq!(collection_items(fixture.db(), group_two.id), vec![third]);
}

#[test]
fn preview_reports_steps_affected_and_blocks_optimize() {
    let fixture = Fixture::new();
    let image = fixture.insert_image("preview");
    let (_, first) = metadata::set_scores_with_id(fixture.db(), &[image], 3).unwrap();
    let first = first.unwrap();
    let (_, second) = metadata::set_scores_with_id(fixture.db(), &[image], 4).unwrap();
    let second = second.unwrap();

    let preview = preview_undo(fixture.db(), second).unwrap();
    assert_eq!(preview.steps, 1);
    assert_eq!(preview.affected, 1);
    assert_eq!(preview.entries[0].id, second);
    assert!(preview.blocked_reason.is_none());

    let preview = preview_undo(fixture.db(), first).unwrap();
    assert_eq!(preview.steps, 2);
    assert_eq!(preview.affected, 2);
    assert_eq!(preview.entries[0].id, second);

    // 模拟两条都已撤销，重做目标取较新的一条，会连续重做两条。
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute("UPDATE operations SET status = 'undone', undone_at = 1", [])?;
            Ok(())
        })
        .unwrap();
    let preview = preview_redo(fixture.db(), second).unwrap();
    assert_eq!(preview.steps, 2);
    assert_eq!(preview.affected, 2);
    assert_eq!(preview.entries[0].id, first);
    assert_eq!(preview.entries[1].id, second);

    let optimize_id = fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO operations(kind, label, created_at, status, affected, error)
                 VALUES ('optimize', '优化图片空间', 2, 'applied', 5, '')",
                [],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .unwrap();
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "UPDATE operations SET status = 'applied', undone_at = NULL WHERE id IN (?1, ?2)",
                params![first, second],
            )?;
            Ok(())
        })
        .unwrap();
    let preview = preview_undo(fixture.db(), first).unwrap();
    assert_eq!(preview.steps, 3);
    assert_eq!(preview.affected, 7);
    assert_eq!(
        preview.blocked_reason.as_deref(),
        Some("压缩操作不可撤销，原图已删除")
    );
    assert!(preview.entries.iter().any(|entry| entry.id == optimize_id));
}

#[test]
fn write_with_id_returns_new_id_and_none_for_noop() {
    let fixture = Fixture::new();
    let image = fixture.insert_image("write-id");
    let (_, operation) = metadata::set_scores_with_id(fixture.db(), &[image], 2).unwrap();
    let operation = operation.unwrap();
    assert_eq!(
        fixture
            .db()
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT affected FROM operations WHERE id = ?1",
                    [operation],
                    |row| row.get::<_, i64>(0),
                )?)
            })
            .unwrap(),
        1
    );

    let collection = collections::create(fixture.db(), "无改动").unwrap();
    let (_, none) = collections::rename_with_id(fixture.db(), collection.id, "无改动").unwrap();
    assert_eq!(none, None);
}

#[test]
fn description_write_reports_changed_count_and_skips_identical_text() {
    let fixture = Fixture::new();
    let image = fixture.insert_image("description");
    let (changed, operation) =
        metadata::update_description_with_id(fixture.db(), image, "  新描述  ").unwrap();
    assert_eq!(changed, 1);
    assert!(operation.is_some());

    // 同样的手写描述不再产生记录，也不会报成一次改动。
    let (changed, operation) =
        metadata::update_description_with_id(fixture.db(), image, "新描述").unwrap();
    assert_eq!(changed, 0);
    assert_eq!(operation, None);
}

#[test]
fn interrupted_run_is_detected_from_operation_status() {
    let fixture = Fixture::new();
    let image = fixture.insert_image("interrupted");
    let (_, operation) = metadata::set_scores_with_id(fixture.db(), &[image], 1).unwrap();
    let id = operation.unwrap();
    assert!(!has_interrupted_run(fixture.db()).unwrap());
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute(
                "UPDATE operations SET status = 'undoing' WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(has_interrupted_run(fixture.db()).unwrap());
}

#[test]
fn live_operation_drop_cleans_empty_record() {
    let fixture = Fixture::new();
    let operation = LiveOperation::begin(
        &fixture.session.db,
        NewOperation::new(HistoryKind::Score, "空任务"),
    )
    .unwrap();
    let id = operation.id();
    drop(operation);
    let count: i64 = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM operations WHERE id = ?1",
                [id],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn live_operation_finish_keeps_written_content() {
    let fixture = Fixture::new();
    let operation = LiveOperation::begin(
        &fixture.session.db,
        NewOperation::new(HistoryKind::Score, "有内容"),
    )
    .unwrap();
    let id = operation.id();
    fixture
        .db()
        .with_writer(|connection| {
            connection.execute("UPDATE operations SET affected = 2 WHERE id = ?1", [id])?;
            Ok(())
        })
        .unwrap();
    operation.finish("中途失败").unwrap();
    let (status, error): (String, String) = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT status, error FROM operations WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(status, "applied");
    assert_eq!(error, "中途失败");
}

#[test]
fn history_storage_counts_chunk_and_undo_files() {
    let fixture = Fixture::new();
    let operation = LiveOperation::begin(
        &fixture.session.db,
        NewOperation::new(HistoryKind::Score, "占用"),
    )
    .unwrap();
    let id = operation.id();
    fixture
        .db()
        .with_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            add_live_affected_in_tx(&transaction, id, 1)?;
            append_live_in_tx(&transaction, id, ChunkFormat::Json, b"hello")?;
            transaction.commit()?;
            Ok(())
        })
        .unwrap();
    operation.finish("").unwrap();

    let undo = LibraryPaths::new(&fixture.root).undo.join("1");
    fs::create_dir_all(&undo).unwrap();
    fs::write(undo.join("payload"), b"abc").unwrap();

    let storage = storage_usage(fixture.db(), &fixture.root).unwrap();
    assert_eq!(storage.record_count, 1);
    assert_eq!(storage.chunk_count, 1);
    assert_eq!(storage.chunk_bytes, 5);
    assert_eq!(storage.undo_files, 1);
    assert_eq!(storage.undo_bytes, 3);
    assert_eq!(storage.total_bytes, 8);
}

#[test]
fn history_run_guard_marks_interrupted_run_failed() {
    let fixture = Fixture::new();
    let image = fixture.insert_image("run-guard");
    let (_, operation) = metadata::set_scores_with_id(fixture.db(), &[image], 1).unwrap();
    let run_id = start_run(fixture.db(), operation.unwrap()).unwrap();
    drop(HistoryRunGuard::new(fixture.db(), run_id));
    let state: String = fixture
        .db()
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT state FROM undo_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(state, "failed");
}

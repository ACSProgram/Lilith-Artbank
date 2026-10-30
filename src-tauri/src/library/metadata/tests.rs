use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{OptionalExtension, params};
use tempfile::tempdir;

use super::{read::prefer_chunked_probe, *};
use crate::library::{
    NoopReporter,
    history::{self, HistoryKind, NewOperation},
    import::ensure_folder,
    paths::LibraryPaths,
    service, temp,
};

struct Fixture {
    _temp: tempfile::TempDir,
    session: service::LibrarySession,
    root: PathBuf,
    folder: i64,
}

fn fixture() -> Fixture {
    let temp = tempdir().unwrap();
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "元数据测试", 1).unwrap();
    let folder = session
        .db
        .with_writer(|connection| ensure_folder(connection, "人物", 1))
        .unwrap();
    Fixture {
        _temp: temp,
        session,
        root,
        folder,
    }
}

fn insert_image(fixture: &Fixture, seed: &str, extension: &str, score: i64, folder_id: i64) -> i64 {
    let hash = format!("{seed:0>64}");
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO images(
                    content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at,
                    modified_at
                 ) VALUES (?1, ?2, ?3, 100, 10, 10, ?2, 0,
                    zeroblob(32), ?4, ?5, ?6, '', 1, 1)",
                params![
                    hash,
                    extension,
                    folder_id,
                    vec![1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
                    vec![20_u8; 32],
                    score
                ],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .unwrap()
}

/// 0 张标签一次事务删完、只写一条记录；撤销把标签行与关系一起恢复。
#[test]
fn deletes_zero_tags_in_one_operation_and_undoes() {
    let fixture = fixture();
    let image_id = insert_image(&fixture, "a", "jpg", 3, fixture.folder);
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO tags(id, name) VALUES (10, '有人用'), (11, '空一'), (12, '空二')",
                [],
            )?;
            connection.execute(
                "INSERT INTO image_tags(image_id, tag_id) VALUES (?1, 10)",
                [image_id],
            )?;
            Ok(())
        })
        .unwrap();

    let removed = delete_zero_tags(&fixture.session.db).unwrap();
    assert_eq!(removed, 2);
    let remaining: Vec<String> = fixture
        .session
        .db
        .with_reader(|connection| {
            let mut statement = connection.prepare("SELECT name FROM tags ORDER BY id")?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(remaining, vec!["有人用".to_owned()]);

    let entries = history::list(&fixture.session.db).unwrap();
    assert_eq!(entries.len(), 1, "一次清理只写一条记录");
    assert_eq!(entries[0].kind, HistoryKind::TagDelete);
    assert_eq!(entries[0].affected, 2);

    let task = crate::app::tasks::TaskHandle::new(
        "undo-zero-tags",
        crate::app::tasks::TaskKind::Undo,
        "测试",
    );
    history::run_undo(
        &fixture.session.db,
        &fixture.root,
        &fixture.session.thumbs,
        entries[0].id,
        task,
    )
    .unwrap();
    let restored: Vec<String> = fixture
        .session
        .db
        .with_reader(|connection| {
            let mut statement = connection.prepare("SELECT name FROM tags ORDER BY id")?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(
        restored,
        vec!["有人用".to_owned(), "空一".to_owned(), "空二".to_owned()]
    );
}

fn write_content(
    fixture: &Fixture,
    folder_path: &str,
    hash_seed: &str,
    extension: &str,
) -> PathBuf {
    let hash = format!("{hash_seed:0>64}");
    let file_name = format!("{hash}.{extension}");
    let path = LibraryPaths::new(&fixture.root).content_file(folder_path, &file_name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"content").unwrap();
    path
}

#[test]
fn image_states_report_existing_and_trashed_rows() {
    let fixture = fixture();
    let active = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    let trashed = insert_image(&fixture, "b", "jpg", 1, fixture.folder);
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute(
                "UPDATE images SET deleted_at = 123 WHERE id = ?1",
                [trashed],
            )?;
            Ok(())
        })
        .unwrap();

    let states = image_states(&fixture.session.db, &[trashed, 999_999, active, active]).unwrap();
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].id, active);
    assert!(states[0].deleted_at.is_none());
    assert_eq!(states[1].id, trashed);
    assert_eq!(states[1].deleted_at, Some(123));

    let too_many: Vec<i64> = (1..=1_001).collect();
    assert!(image_states(&fixture.session.db, &too_many).is_err());
}

#[test]
fn selection_tags_reports_common_and_partial_counts() {
    let fixture = fixture();
    let first = insert_image(&fixture, "s1", "jpg", 0, fixture.folder);
    let second = insert_image(&fixture, "s2", "jpg", 0, fixture.folder);
    let third = insert_image(&fixture, "s3", "jpg", 0, fixture.folder);
    set_image_tags(&fixture.session.db, first, &["共同".into(), "仅一".into()]).unwrap();
    set_image_tags(&fixture.session.db, second, &["共同".into(), "甲".into()]).unwrap();
    set_image_tags(&fixture.session.db, third, &["共同".into(), "乙".into()]).unwrap();

    let result = selection_tags(&fixture.session.db, &[first, second, third, first]).unwrap();
    assert_eq!(result.total, 3);
    assert_eq!(result.tags[0].name, "共同");
    assert_eq!(result.tags[0].count, 3);
    let partial: Vec<_> = result.tags.iter().filter(|tag| tag.count < 3).collect();
    assert_eq!(partial.len(), 3);
    assert!(partial.iter().all(|tag| tag.count == 1));
}

#[test]
fn selection_tags_uses_chunk_path_for_dense_selection() {
    let fixture = fixture();
    let first = insert_image(&fixture, "d1", "jpg", 0, fixture.folder);
    let second = insert_image(&fixture, "d2", "jpg", 0, fixture.folder);
    let first_tags: Vec<String> = (0..64).map(|index| format!("标签{index:02}")).collect();
    let second_tags: Vec<String> = (32..96).map(|index| format!("标签{index:02}")).collect();
    set_image_tags(&fixture.session.db, first, &first_tags).unwrap();
    set_image_tags(&fixture.session.db, second, &second_tags).unwrap();

    let result = selection_tags(&fixture.session.db, &[first, second]).unwrap();
    assert_eq!(result.total, 2);
    assert_eq!(result.tags.len(), 96);
    assert_eq!(result.tags.iter().filter(|tag| tag.count == 2).count(), 32);
    assert_eq!(result.tags.iter().filter(|tag| tag.count == 1).count(), 64);
}

#[test]
fn selection_tags_handles_empty_and_invalid_ids() {
    let fixture = fixture();
    let empty = selection_tags(&fixture.session.db, &[]).unwrap();
    assert_eq!(empty.total, 0);
    assert!(empty.tags.is_empty());
    assert!(selection_tags(&fixture.session.db, &[0]).is_err());

    let too_many: Vec<i64> = (1..=1_000_001).collect();
    assert!(selection_tags(&fixture.session.db, &too_many).is_err());
}

#[test]
fn chunked_probe_only_wins_for_sparse_relations() {
    // 60 万条关系：2 万选中走分块，20 万选中走扫描。
    assert!(prefer_chunked_probe(600_000, 20_000));
    assert!(!prefer_chunked_probe(600_000, 200_000));
    assert!(!prefer_chunked_probe(0, 0));
}

#[test]
fn reads_and_updates_metadata() {
    let fixture = fixture();
    let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
    assert_eq!(details.score, 1);
    assert_eq!(details.colors4.len(), 4);
    assert_eq!(details.colors4[0].weight, 4);
    assert_eq!(details.colors8.len(), 8);
    assert!(
        details
            .file_path
            .ends_with(&format!("{}.jpg", "0".repeat(63) + "a"))
    );

    update_score(&fixture.session.db, image_id, 5).unwrap();
    update_description(&fixture.session.db, image_id, "  人物速写  ").unwrap();

    let tags = set_image_tags(
        &fixture.session.db,
        image_id,
        &["速写".into(), "参考".into()],
    )
    .unwrap();
    assert_eq!(tags.len(), 2);

    let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
    assert_eq!(details.score, 5);
    assert_eq!(details.description, "人物速写");
    assert_eq!(details.description_source, "user");
    assert_eq!(details.tags.len(), 2);

    let found: i64 = fixture
        .session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM image_search WHERE image_search MATCH '\"人物速\"'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(found, 1);
}

#[test]
fn temp_metadata_changes_do_not_write_history() {
    let fixture = fixture();
    let temp_folder = fixture
        .session
        .db
        .with_reader(temp::folder)
        .unwrap()
        .unwrap()
        .id;
    let temp_image = insert_image(&fixture, "t", "jpg", 0, temp_folder);
    let normal_image = insert_image(&fixture, "n", "jpg", 0, fixture.folder);

    update_score(&fixture.session.db, temp_image, 5).unwrap();
    update_description(&fixture.session.db, temp_image, "临时描述").unwrap();
    set_image_tags(&fixture.session.db, temp_image, &["临时".into()]).unwrap();
    assert!(history::list(&fixture.session.db).unwrap().is_empty());

    set_scores(&fixture.session.db, &[temp_image, normal_image], 4).unwrap();
    let entries = history::list(&fixture.session.db).unwrap();
    assert_eq!(entries.len(), 1);

    let details = image_details(&fixture.session.db, &fixture.root, temp_image).unwrap();
    assert_eq!(details.score, 4);
    assert_eq!(details.description, "临时描述");
}

#[test]
fn edits_tags_in_batches_and_renames() {
    let fixture = fixture();
    let first = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    let second = insert_image(&fixture, "b", "png", 1, fixture.folder);

    assert_eq!(
        add_tags(
            &fixture.session.db,
            &[first, second],
            &["速写".into(), "练习".into()]
        )
        .unwrap(),
        4
    );
    assert_eq!(
        remove_tags(&fixture.session.db, &[first], &["练习".into()]).unwrap(),
        1
    );
    assert_eq!(
        image_details(&fixture.session.db, &fixture.root, first)
            .unwrap()
            .tags
            .len(),
        1
    );

    let tag_id = image_details(&fixture.session.db, &fixture.root, first)
        .unwrap()
        .tags[0]
        .id;
    rename_tag(&fixture.session.db, tag_id, "草稿").unwrap();
    let names: Vec<String> = image_details(&fixture.session.db, &fixture.root, second)
        .unwrap()
        .tags
        .into_iter()
        .map(|tag| tag.name)
        .collect();
    assert!(names.contains(&"草稿".to_owned()));

    let affected = delete_tag(&fixture.session.db, tag_id).unwrap();
    assert_eq!(affected, 2);
    assert_eq!(
        image_details(&fixture.session.db, &fixture.root, first)
            .unwrap()
            .tags
            .len(),
        0
    );
}

#[test]
fn moves_files_between_folders_and_rolls_back_failures() {
    let fixture = fixture();
    let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    let old_path = write_content(&fixture, "人物", "a", "jpg");

    let report = move_images(
        &fixture.session.db,
        &fixture.root,
        &[image_id],
        "风景/子目录",
        &NoopReporter,
    )
    .unwrap();
    assert_eq!(report.moved, 1);
    assert!(report.failures.is_empty());
    assert!(!old_path.exists());
    let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
    assert_eq!(details.folder_path, "风景/子目录");
    assert!(Path::new(&details.file_path).exists());

    let report = move_images(
        &fixture.session.db,
        &fixture.root,
        &[image_id],
        "风景/子目录",
        &NoopReporter,
    )
    .unwrap();
    assert_eq!(report.skipped, 1);

    // 目标位置被目录占用时，重命名失败；数据库仍指向原文件夹。
    let blocked = insert_image(&fixture, "b", "png", 1, fixture.folder);
    let blocked_path = write_content(&fixture, "人物", "b", "png");
    let target = LibraryPaths::new(&fixture.root)
        .content_file("风景/子目录", &format!("{}.png", "0".repeat(63) + "b"));
    fs::create_dir_all(&target).unwrap();
    let report = move_images(
        &fixture.session.db,
        &fixture.root,
        &[blocked],
        "风景/子目录",
        &NoopReporter,
    )
    .unwrap();
    assert_eq!(report.moved, 0);
    assert_eq!(report.failures.len(), 1);
    assert!(blocked_path.exists());
    assert_eq!(
        image_details(&fixture.session.db, &fixture.root, blocked)
            .unwrap()
            .folder_path,
        "人物"
    );
}

#[test]
fn trashes_restores_and_permanently_deletes() {
    let fixture = fixture();
    let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    let path = write_content(&fixture, "人物", "a", "jpg");
    fixture
        .session
        .db
        .with_writer(|connection| {
            connection.execute("INSERT INTO tags(name) VALUES ('速写')", [])?;
            connection.execute(
                "INSERT INTO collections(id, name, sort_order, created_at, updated_at)
                 VALUES (1, '集合', 0, 1, 1)",
                [],
            )?;
            connection.execute(
                "INSERT INTO collection_items(collection_id, image_id, position)
                 VALUES (1, ?1, 0)",
                [image_id],
            )?;
            connection.execute(
                "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name, pack_offset, pack_length, updated_at)
                 VALUES (?1, 'embedding', 'm', 3, 'f.bin', 0, 12, 1)",
                [image_id],
            )?;
            connection.execute(
                "INSERT INTO image_tags(image_id, tag_id)
                 SELECT ?1, id FROM tags WHERE name = '速写'",
                [image_id],
            )?;
            Ok(())
        })
        .unwrap();

    assert_eq!(
        set_trashed(&fixture.session.db, &[image_id], true).unwrap(),
        1
    );
    let details = image_details(&fixture.session.db, &fixture.root, image_id).unwrap();
    assert!(details.deleted_at.is_some());
    assert!(path.exists());
    assert_eq!(
        set_trashed(&fixture.session.db, &[image_id], false).unwrap(),
        1
    );
    assert!(
        image_details(&fixture.session.db, &fixture.root, image_id)
            .unwrap()
            .deleted_at
            .is_none()
    );
    // 永久删除只接受回收站里的图片，避免绕过界面隐藏正常图片。
    assert!(delete_images(&fixture.session.db, &[image_id], &NoopReporter).is_err());
    set_trashed(&fixture.session.db, &[image_id], true).unwrap();

    let report = delete_images(&fixture.session.db, &[image_id], &NoopReporter).unwrap();
    assert!(report.deferred);
    assert_eq!(report.deleted, 1);
    assert_eq!(report.files_removed, 0);
    assert!(path.exists());
    assert!(
        image_details(&fixture.session.db, &fixture.root, image_id)
            .unwrap()
            .deleted_at
            .is_some()
    );

    let operation = history::list(&fixture.session.db)
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == HistoryKind::PermanentDelete)
        .unwrap();
    let report = purge_by_operation(
        &fixture.session.db,
        &fixture.root,
        &[operation.id],
        &NoopReporter,
    )
    .unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(report.files_removed, 1);
    assert!(!path.exists());
    assert!(image_details(&fixture.session.db, &fixture.root, image_id).is_err());

    let leftovers: (i64, i64, i64, i64) = fixture
        .session
        .db
        .with_reader(|connection| {
            Ok((
                connection.query_row("SELECT count(*) FROM image_tags", [], |row| row.get(0))?,
                connection.query_row("SELECT count(*) FROM collection_items", [], |row| {
                    row.get(0)
                })?,
                connection.query_row("SELECT count(*) FROM feature_blobs", [], |row| row.get(0))?,
                connection.query_row("SELECT count(*) FROM image_search", [], |row| row.get(0))?,
            ))
        })
        .unwrap();
    assert_eq!(leftovers, (0, 0, 0, 0));
}

#[test]
fn empties_trash_only() {
    let fixture = fixture();
    let active = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    let trashed = insert_image(&fixture, "b", "jpg", 1, fixture.folder);
    write_content(&fixture, "人物", "a", "jpg");
    let trashed_path = write_content(&fixture, "人物", "b", "jpg");
    set_trashed(&fixture.session.db, &[trashed], true).unwrap();

    let report = empty_trash(&fixture.session.db, &NoopReporter).unwrap();
    assert!(report.deferred);
    assert_eq!(report.deleted, 1);
    assert_eq!(report.files_removed, 0);
    assert!(trashed_path.exists());
    assert!(image_details(&fixture.session.db, &fixture.root, active).is_ok());
    assert!(
        image_details(&fixture.session.db, &fixture.root, trashed)
            .unwrap()
            .deleted_at
            .is_some()
    );

    let operation = history::list(&fixture.session.db)
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == HistoryKind::EmptyTrash)
        .unwrap();
    purge_by_operation(
        &fixture.session.db,
        &fixture.root,
        &[operation.id],
        &NoopReporter,
    )
    .unwrap();
    assert!(!trashed_path.exists());
    assert!(image_details(&fixture.session.db, &fixture.root, trashed).is_err());
}

/// 路径无效的行删掉就再也查不到：保留行、清掉待释放标记，并在报告里给出原路径。
#[test]
fn release_keeps_rows_whose_content_path_is_invalid() {
    let fixture = fixture();
    let hash = "b".repeat(64);
    let broken = fixture
        .session
        .db
        .with_writer(|connection| {
            // 扩展名没规范化：`content_path` 会拒绝，这类行永远定位不到原图。
            connection.execute(
                "INSERT INTO images(
                    content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at,
                    modified_at, deleted_at
                 ) VALUES (?1, 'JPG', ?2, 100, 10, 10, 'jpeg', 0, zeroblob(32),
                    zeroblob(16), zeroblob(32), 0, '', 1, 1, 1)",
                params![hash, fixture.folder],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .unwrap();
    let (_, operation) = history::write_with_id(
        &fixture.session.db,
        NewOperation::new(HistoryKind::PermanentDelete, "永久删除"),
        |transaction, draft| {
            transaction.execute(
                "UPDATE images SET purge_operation_id = ?1 WHERE id = ?2",
                params![draft.id(), broken],
            )?;
            draft.affected(1);
            Ok(())
        },
    )
    .unwrap();
    let operation = operation.unwrap();

    let report = purge_by_operation(
        &fixture.session.db,
        &fixture.root,
        &[operation],
        &NoopReporter,
    )
    .unwrap();
    assert_eq!(report.deleted, 0);
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.failures.len(), 1);
    assert!(
        report.failures[0].path.contains(&hash),
        "失败清单要给出原路径：{:?}",
        report.failures[0].path
    );

    let (rows, purge): (i64, Option<i64>) = fixture
        .session
        .db
        .with_reader(|connection| {
            Ok((
                connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?,
                connection
                    .query_row(
                        "SELECT purge_operation_id FROM images WHERE id = ?1",
                        [broken],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten(),
            ))
        })
        .unwrap();
    assert_eq!(rows, 1, "行保留，仍能在回收站里看到");
    assert!(purge.is_none(), "待释放标记要清掉，否则永远释放不完");
}

/// 恢复图片的守卫在写事务里：被拒时不能留下一条空的操作记录。
#[test]
fn rejecting_a_restore_from_a_trashed_folder_leaves_no_record() {
    let fixture = fixture();
    let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    crate::library::folders::trash(&fixture.session.db, fixture.folder).unwrap();
    let before = history::list(&fixture.session.db).unwrap().len();

    assert!(set_trashed(&fixture.session.db, &[image_id], false).is_err());
    assert_eq!(
        history::list(&fixture.session.db).unwrap().len(),
        before,
        "被拒的恢复不该写记录"
    );
}

fn tag_pair_count(fixture: &Fixture, tag_id: i64) -> u64 {
    fixture
        .session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM image_tags WHERE tag_id = ?1",
                [tag_id],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .unwrap() as u64
}

fn undo_task() -> std::sync::Arc<crate::app::tasks::TaskHandle> {
    crate::app::tasks::TaskHandle::new("merge-test", crate::app::tasks::TaskKind::Undo, "测试")
}

#[test]
fn merge_moves_relations_and_can_be_undone() {
    let fixture = fixture();
    let a = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    let b = insert_image(&fixture, "b", "jpg", 1, fixture.folder);
    let c = insert_image(&fixture, "c", "jpg", 1, fixture.folder);
    add_tags(&fixture.session.db, &[a, b], &["源".into()]).unwrap();
    add_tags(&fixture.session.db, &[b, c], &["目标".into()]).unwrap();
    let (source_id, target_id) = fixture
        .session
        .db
        .with_reader(|connection| {
            Ok((
                connection.query_row("SELECT id FROM tags WHERE name = '源'", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                connection.query_row("SELECT id FROM tags WHERE name = '目标'", [], |row| {
                    row.get::<_, i64>(0)
                })?,
            ))
        })
        .unwrap();

    let preview = merge_preview(&fixture.session.db, source_id, target_id).unwrap();
    assert_eq!(preview.source_images, 2);
    assert_eq!(preview.moved, 1);
    assert_eq!(preview.duplicate, 1);

    let (report, operation_id) =
        merge_tags_with_id(&fixture.session.db, source_id, target_id).unwrap();
    assert_eq!(report.moved, 1);
    assert_eq!(report.duplicate, 1);
    assert!(operation_id.is_some());
    assert_eq!(tag_pair_count(&fixture, target_id), 3);
    assert!(tag_name(&fixture.session.db, source_id).unwrap().is_none());

    let operation_id = operation_id.unwrap();
    history::run_undo(
        &fixture.session.db,
        &fixture.root,
        &fixture.session.thumbs,
        operation_id,
        undo_task(),
    )
    .unwrap();
    assert_eq!(
        tag_name(&fixture.session.db, source_id).unwrap().as_deref(),
        Some("源")
    );
    assert_eq!(tag_pair_count(&fixture, source_id), 2);
    assert_eq!(tag_pair_count(&fixture, target_id), 2);

    history::run_redo(
        &fixture.session.db,
        &fixture.root,
        &fixture.session.thumbs,
        operation_id,
        undo_task(),
    )
    .unwrap();
    assert!(tag_name(&fixture.session.db, source_id).unwrap().is_none());
    assert_eq!(tag_pair_count(&fixture, target_id), 3);
}

#[test]
fn rejects_invalid_inputs() {
    let fixture = fixture();
    let image_id = insert_image(&fixture, "a", "jpg", 1, fixture.folder);
    assert!(update_score(&fixture.session.db, image_id, 6).is_err());
    assert!(update_description(&fixture.session.db, image_id, &"x".repeat(50_001)).is_err());
    assert!(rename_tag(&fixture.session.db, 1, "").is_err());
    assert!(
        move_images(
            &fixture.session.db,
            &fixture.root,
            &[],
            "目标",
            &NoopReporter
        )
        .is_err()
    );
    assert!(
        move_images(
            &fixture.session.db,
            &fixture.root,
            &[image_id],
            "../外面",
            &NoopReporter
        )
        .is_err()
    );
}

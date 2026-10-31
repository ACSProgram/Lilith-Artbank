use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use tempfile::tempdir;

use super::*;
use crate::library::{
    db::LibraryDb,
    history::{self, HistoryKind},
    paths::{LibraryPaths, parent_path},
    query, service,
};
fn fixture() -> (tempfile::TempDir, Arc<LibraryDb>, PathBuf) {
    let temp = tempdir().unwrap();
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "文件夹测试", 1).unwrap();
    let database = session.db.clone();
    (temp, database, root)
}

#[test]
fn creates_nested_folders_with_ancestors() {
    let (_temp, db, root) = fixture();
    let folders = create(&db, &root, "人物/速写/手部").unwrap();
    let paths: Vec<&str> = folders.iter().map(|item| item.path.as_str()).collect();
    assert!(paths.contains(&"人物"));
    assert!(paths.contains(&"人物/速写"));
    assert!(paths.contains(&"人物/速写/手部"));
    assert!(root.join("人物").join("速写").join("手部").is_dir());
    assert!(create(&db, &root, "人物").is_err());
    assert!(create(&db, &root, "CON").is_err());

    let entries = history::list(&db).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, HistoryKind::FolderCreate);
    assert_eq!(entries[0].affected, 3);
}

#[test]
fn renames_and_moves_subtrees() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "人物/速写").unwrap();
    let people = db
        .with_reader(|connection| folder_id(connection, "人物"))
        .unwrap()
        .unwrap();
    let sketch = db
        .with_reader(|connection| folder_id(connection, "人物/速写"))
        .unwrap()
        .unwrap();
    let scenery = create(&db, &root, "风景").unwrap();
    let scenery_id = scenery
        .iter()
        .find(|item| item.path == "风景")
        .map(|item| item.id)
        .unwrap();

    rename(&db, &root, sketch, "草稿").unwrap();
    assert!(root.join("人物").join("草稿").is_dir());
    assert!(!root.join("人物").join("速写").exists());
    let sketch = db
        .with_reader(|connection| folder_id(connection, "人物/草稿"))
        .unwrap()
        .unwrap();

    move_to(&db, &root, sketch, scenery_id).unwrap();
    assert!(root.join("风景").join("草稿").is_dir());
    let moved = db
        .with_reader(|connection| folder_id(connection, "风景/草稿"))
        .unwrap();
    assert_eq!(moved, Some(sketch));

    assert!(move_to(&db, &root, people, people).is_err());
    assert!(move_to(&db, &root, people, scenery_id).is_ok());
}

#[test]
fn renames_case_only_and_rejects_conflicts() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "Sketches").unwrap();
    let id = db
        .with_reader(|connection| folder_id(connection, "Sketches"))
        .unwrap()
        .unwrap();
    rename(&db, &root, id, "sketches").unwrap();
    let names: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|name| name == "sketches"));
    assert!(!names.iter().any(|name| name == "Sketches"));
    assert!(create(&db, &root, "SKETCHES").is_err());
}

#[test]
fn reorders_siblings_and_rejects_foreign_folders() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "一").unwrap();
    create(&db, &root, "二").unwrap();
    create(&db, &root, "三").unwrap();
    create(&db, &root, "组/子").unwrap();
    let id = |path: &str| {
        db.with_reader(|connection| folder_id(connection, path))
            .unwrap()
            .unwrap()
    };
    let root_id = id("");
    let (one, two, three) = (id("一"), id("二"), id("三"));
    let group_child = id("组/子");

    let order = |db: &LibraryDb| {
        db.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT path FROM folders
                     WHERE kind = 'normal' AND instr(path, '/') = 0 AND path <> ''
                     ORDER BY sort_index, id",
            )?;
            let paths = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(paths)
        })
        .unwrap()
    };

    reorder(&db, &[three], root_id, 0).unwrap();
    assert_eq!(order(&db), vec!["三", "一", "二", "组"]);

    reorder(&db, &[one], root_id, 10).unwrap();
    reorder(&db, &[two], root_id, 0).unwrap();
    assert_eq!(order(&db), vec!["二", "三", "组", "一"]);

    assert!(reorder(&db, &[group_child], root_id, 0).is_err());
    assert!(reorder(&db, &[one, two], three, 0).is_err());
}

#[test]
fn trashes_whole_subtree_and_restores_it() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "父/子").unwrap();
    let parent = id_of(&db, "父");
    let child = id_of(&db, "父/子");

    let change = trash(&db, &root, parent).unwrap();
    assert_eq!(change.removed_ids.len(), 2);
    assert!(!root.join("父").exists(), "原路径立即释放");
    let storage = stored_path(&db, parent);
    assert!(root.join(&storage).join("子").is_dir(), "子树整体搬入仓库");
    let trash_folders = query::list_trash_folders(&db).unwrap();
    assert_eq!(trash_folders.folders.len(), 2);
    assert!(
        trash_folders
            .folders
            .iter()
            .any(|folder| folder.path == "父" && folder.storage_path.as_deref() == Some(&storage))
    );
    assert!(
        !query::list_folders(&db)
            .unwrap()
            .iter()
            .any(|folder| folder.path == "父" || folder.path == "父/子"),
        "正常树看不到回收站文件夹"
    );

    restore(&db, &root, child).unwrap();
    assert!(
        query::list_folders(&db)
            .unwrap()
            .iter()
            .any(|folder| folder.path == "父/子"),
        "恢复子树里的文件夹会带回整个删除单元"
    );
    assert!(root.join("父").join("子").is_dir(), "恢复会搬回原路径");
    assert!(query::list_trash_folders(&db).unwrap().folders.is_empty());

    trash(&db, &root, parent).unwrap();
    let change = delete_permanently(&db, parent).unwrap();
    assert_eq!(change.removed_ids.len(), 2);
    assert!(query::list_trash_folders(&db).unwrap().folders.is_empty());
    let purged: i64 = db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM folders
                 WHERE id IN (?1, ?2) AND purge_operation_id IS NOT NULL",
                [parent, child],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(purged, 2, "永久删除只写待释放标记，行等释放任务清理");
}

#[test]
fn after_trash_the_original_path_can_be_reused_and_restore_rejects_conflicts() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "父/子").unwrap();
    let parent = id_of(&db, "父");

    trash(&db, &root, parent).unwrap();
    create(&db, &root, "父").unwrap();
    assert!(root.join("父").is_dir(), "删除后可以立即建同名文件夹");

    let error = restore(&db, &root, parent).unwrap_err();
    assert!(
        error.to_string().contains("原位置已经有同名文件夹"),
        "{error}"
    );
    assert!(stored_path(&db, parent).contains(".artbank/trash/"));
}

#[test]
fn undo_and_redo_folder_restore_move_the_whole_unit() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "父/子").unwrap();
    let parent = id_of(&db, "父");
    trash(&db, &root, parent).unwrap();
    restore(&db, &root, parent).unwrap();
    assert!(root.join("父").join("子").is_dir());

    let target = undo_last(&db, &root);
    assert!(!root.join("父").exists(), "撤销恢复会重新搬入仓库");
    assert!(stored_path(&db, parent).contains(".artbank/trash/"));

    redo(&db, &root, target);
    assert!(root.join("父").join("子").is_dir(), "重做恢复会搬回原路径");
    assert_eq!(stored_path(&db, parent), "父");
}

#[test]
fn restores_v10_in_place_trash_rows_without_moving_directories() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "旧目录").unwrap();
    let folder = id_of(&db, "旧目录");
    db.with_writer(|connection| {
        connection.execute("UPDATE folders SET trashed_at = 5 WHERE id = ?1", [folder])?;
        connection.execute(
            "INSERT INTO images(
                content_hash, file_ext, folder_id, file_size, width, height, format,
                has_alpha, phash, colors4, colors8, score, description, imported_at,
                modified_at, deleted_at
             ) VALUES (?1, 'png', ?2, 10, 1, 1, 'png', 0,
                zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1, 5)",
            rusqlite::params!["d".repeat(64), folder],
        )?;
        Ok(())
    })
    .unwrap();

    restore(&db, &root, folder).unwrap();
    assert!(root.join("旧目录").is_dir(), "旧行不搬目录");
    let (path, trashed): (String, Option<i64>) = db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT path, trashed_at FROM folders WHERE id = ?1",
                [folder],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(path, "旧目录");
    assert_eq!(trashed, None);
    let deleted: Option<i64> = db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT deleted_at FROM images WHERE folder_id = ?1",
                [folder],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(deleted, None, "同一次删除的图片一起恢复");
}

#[test]
fn startup_reconcile_uses_the_database_as_the_source_of_truth() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "未提交").unwrap();
    let uncommitted = id_of(&db, "未提交");
    let paths = LibraryPaths::new(&root);
    let warehouse = paths.folder_dir(&format!(".artbank/trash/11/{uncommitted}"));
    fs::create_dir_all(warehouse.parent().unwrap()).unwrap();
    fs::rename(root.join("未提交"), &warehouse).unwrap();

    create(&db, &root, "已提交").unwrap();
    let committed = id_of(&db, "已提交");
    trash(&db, &root, committed).unwrap();
    let stored = stored_path(&db, committed);
    fs::rename(paths.folder_dir(&stored), root.join("已提交")).unwrap();

    reconcile_trash(&db, &root).unwrap();
    assert!(root.join("未提交").is_dir(), "未提交的删除要撤回");
    assert!(!warehouse.exists());
    assert!(!root.join("已提交").exists());
    let restored_storage = stored_path(&db, committed);
    assert!(
        paths.folder_dir(&restored_storage).is_dir(),
        "已提交的删除要补搬"
    );
    assert!(restored_storage.contains(".artbank/trash/"));
    assert_eq!(stored_path(&db, uncommitted), "未提交");
}

fn stored_path(db: &LibraryDb, id: i64) -> String {
    db.with_reader(|connection| {
        Ok(
            connection.query_row("SELECT path FROM folders WHERE id = ?1", [id], |row| {
                row.get(0)
            })?,
        )
    })
    .unwrap()
}

#[test]
fn restore_keeps_separate_deletion_units_apart() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "父/子").unwrap();
    let parent = id_of(&db, "父");
    let child = id_of(&db, "父/子");

    trash(&db, &root, child).unwrap();
    trash(&db, &root, parent).unwrap();
    assert!(
        restore(&db, &root, child).is_err(),
        "上级文件夹来自另一次删除时不能单独恢复"
    );

    restore(&db, &root, parent).unwrap();
    assert!(
        query::list_trash_folders(&db)
            .unwrap()
            .folders
            .iter()
            .any(|folder| folder.path == "父/子"),
        "更早单独删除的子文件夹不随上级恢复"
    );
    restore(&db, &root, child).unwrap();
    assert!(query::list_trash_folders(&db).unwrap().folders.is_empty());
}

#[test]
fn undo_restores_created_renamed_moved_deleted_and_reordered_folders() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "人物/速写").unwrap();
    let people = db
        .with_reader(|connection| folder_id(connection, "人物"))
        .unwrap()
        .unwrap();
    let sketch = db
        .with_reader(|connection| folder_id(connection, "人物/速写"))
        .unwrap()
        .unwrap();
    rename(&db, &root, sketch, "草稿").unwrap();
    let scenery = create(&db, &root, "风景").unwrap();
    let scenery_id = scenery
        .iter()
        .find(|item| item.path == "风景")
        .map(|item| item.id)
        .unwrap();
    move_to(&db, &root, sketch, scenery_id).unwrap();
    let root_id = db
        .with_reader(|connection| folder_id(connection, ""))
        .unwrap()
        .unwrap();
    reorder(&db, &[scenery_id, people], root_id, 0).unwrap();
    trash(&db, &root, sketch).unwrap();

    let entries = history::list(&db).unwrap();
    let target = entries.last().unwrap().id;
    let thumbs = crate::library::thumbs::ThumbPack::open(
        crate::library::paths::LibraryPaths::new(&root).thumbs,
    )
    .unwrap();
    let task = crate::app::tasks::TaskHandle::new(
        "folder-undo",
        crate::app::tasks::TaskKind::Undo,
        "测试",
    );
    history::run_undo(&db, &root, &thumbs, target, task).unwrap();

    let entries = history::list(&db).unwrap();
    assert_eq!(entries.len(), 6);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == crate::library::history::OperationStatus::Undone)
    );
    assert!(!root.join("人物").exists());
    assert!(!root.join("风景").exists());
    assert!(
        db.with_reader(|connection| folder_id(connection, "人物/速写"))
            .unwrap()
            .is_none()
    );

    let target = entries[0].id;
    let task = crate::app::tasks::TaskHandle::new(
        "folder-redo",
        crate::app::tasks::TaskKind::Redo,
        "测试",
    );
    history::run_redo(&db, &root, &thumbs, target, task).unwrap();
    let entries = history::list(&db).unwrap();
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == crate::library::history::OperationStatus::Applied)
    );
    assert!(root.join("人物").is_dir());
    assert!(root.join("风景").is_dir());
    assert!(
        !root.join("风景").join("草稿").exists(),
        "重做删除会把目录搬入回收站仓库"
    );
    assert!(stored_path(&db, sketch).contains(".artbank/trash/"));
    assert!(!root.join("人物").join("速写").exists());
    assert!(!root.join("人物").join("草稿").exists());
    let order: Vec<String> = db
        .with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT path FROM folders
                     WHERE kind = 'normal' AND instr(path, '/') = 0 AND path <> ''
                     ORDER BY sort_index, id",
            )?;
            let paths = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(paths)
        })
        .unwrap();
    assert_eq!(order, vec!["风景".to_owned(), "人物".to_owned()]);
}

fn id_of(db: &LibraryDb, path: &str) -> i64 {
    db.with_reader(|connection| folder_id(connection, path))
        .unwrap()
        .unwrap()
}

fn children(db: &LibraryDb, parent: &str) -> Vec<String> {
    db.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT path FROM folders WHERE kind = 'normal' AND path <> '' ORDER BY sort_index, id",
        )?;
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(paths)
    })
    .unwrap()
    .into_iter()
    .filter(|path| parent_path(path) == parent)
    .collect()
}

fn undo_last(db: &LibraryDb, root: &Path) -> i64 {
    let target = history::list(db).unwrap().first().unwrap().id;
    let thumbs = crate::library::thumbs::ThumbPack::open(
        crate::library::paths::LibraryPaths::new(root).thumbs,
    )
    .unwrap();
    let task = crate::app::tasks::TaskHandle::new(
        "folder-test-undo",
        crate::app::tasks::TaskKind::Undo,
        "测试",
    );
    history::run_undo(db, root, &thumbs, target, task).unwrap();
    target
}

fn redo(db: &LibraryDb, root: &Path, target: i64) {
    let thumbs = crate::library::thumbs::ThumbPack::open(
        crate::library::paths::LibraryPaths::new(root).thumbs,
    )
    .unwrap();
    let task = crate::app::tasks::TaskHandle::new(
        "folder-test-redo",
        crate::app::tasks::TaskKind::Redo,
        "测试",
    );
    history::run_redo(db, root, &thumbs, target, task).unwrap();
}

#[test]
fn places_multiple_folders_in_one_operation_and_undoes() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "甲").unwrap();
    create(&db, &root, "乙").unwrap();
    create(&db, &root, "目标").unwrap();
    let (甲, 乙, 目标) = (id_of(&db, "甲"), id_of(&db, "乙"), id_of(&db, "目标"));

    place_many(&db, &root, &[甲, 乙], 目标, None).unwrap();
    assert!(root.join("目标").join("甲").is_dir());
    assert!(root.join("目标").join("乙").is_dir());
    assert!(!root.join("甲").exists());
    assert_eq!(children(&db, "目标"), vec!["目标/甲", "目标/乙"]);
    let entries = history::list(&db).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].kind, HistoryKind::FolderMove);
    assert!(entries[0].label.contains("目标"));

    let target = undo_last(&db, &root);
    assert!(root.join("甲").is_dir());
    assert!(root.join("乙").is_dir());
    assert!(!root.join("目标").join("甲").exists());
    assert_eq!(children(&db, ""), vec!["甲", "乙", "目标"]);

    redo(&db, &root, target);
    assert!(root.join("目标").join("甲").is_dir());
    assert_eq!(children(&db, "目标"), vec!["目标/甲", "目标/乙"]);
}

#[test]
fn places_multiple_folders_at_a_position_and_restores_sibling_order() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "目标/一").unwrap();
    create(&db, &root, "目标/二").unwrap();
    create(&db, &root, "甲").unwrap();
    create(&db, &root, "乙").unwrap();
    let (甲, 乙, 目标) = (id_of(&db, "甲"), id_of(&db, "乙"), id_of(&db, "目标"));

    place_many(&db, &root, &[甲, 乙], 目标, Some(1)).unwrap();
    assert_eq!(
        children(&db, "目标"),
        vec!["目标/一", "目标/甲", "目标/乙", "目标/二"]
    );

    let target = undo_last(&db, &root);
    assert_eq!(children(&db, "目标"), vec!["目标/一", "目标/二"]);
    assert_eq!(children(&db, ""), vec!["目标", "甲", "乙"]);

    redo(&db, &root, target);
    assert_eq!(
        children(&db, "目标"),
        vec!["目标/一", "目标/甲", "目标/乙", "目标/二"]
    );
}

#[test]
fn place_many_keeps_only_outermost_selected_folders() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "父/子").unwrap();
    create(&db, &root, "目标").unwrap();
    let 父 = id_of(&db, "父");
    let 子 = id_of(&db, "父/子");
    let 目标 = id_of(&db, "目标");
    place_many(&db, &root, &[父, 子], 目标, None).unwrap();
    assert!(root.join("目标").join("父").join("子").is_dir());
    assert_eq!(children(&db, "目标"), vec!["目标/父"]);
}

#[test]
fn applies_folder_order_in_one_record_and_undoes() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "丙").unwrap();
    create(&db, &root, "甲").unwrap();
    create(&db, &root, "乙").unwrap();
    let root_id = id_of(&db, "");
    let (甲, 乙, 丙) = (id_of(&db, "甲"), id_of(&db, "乙"), id_of(&db, "丙"));
    let before = children(&db, "");

    apply_order(
        &db,
        root_id,
        FolderSortKey::Name,
        false,
        false,
        &[(root_id, vec![甲, 乙, 丙])],
    )
    .unwrap();
    assert_eq!(children(&db, ""), vec!["甲", "乙", "丙"]);
    let entries = history::list(&db).unwrap();
    assert_eq!(entries[0].kind, HistoryKind::FolderReorder);
    assert!(entries[0].label.contains("按名称"));
    assert_eq!(entries[0].affected, 3);

    let target = undo_last(&db, &root);
    assert_eq!(children(&db, ""), before);
    redo(&db, &root, target);
    assert_eq!(children(&db, ""), vec!["甲", "乙", "丙"]);
}

#[test]
fn apply_order_rejects_stale_or_foreign_children() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "甲").unwrap();
    create(&db, &root, "乙").unwrap();
    create(&db, &root, "组/子").unwrap();
    let root_id = id_of(&db, "");
    let (甲, 乙) = (id_of(&db, "甲"), id_of(&db, "乙"));
    let 子 = id_of(&db, "组/子");

    assert!(
        apply_order(
            &db,
            root_id,
            FolderSortKey::Total,
            true,
            false,
            &[(root_id, vec![甲])],
        )
        .is_err()
    );
    assert!(
        apply_order(
            &db,
            root_id,
            FolderSortKey::Name,
            false,
            false,
            &[(root_id, vec![甲, 乙, 子])],
        )
        .is_err()
    );
    let 组 = id_of(&db, "组");
    assert!(
        apply_order(
            &db,
            root_id,
            FolderSortKey::Name,
            false,
            false,
            &[(组, vec![子])],
        )
        .is_err()
    );
}

#[test]
fn move_and_delete_labels_include_paths() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "父/子").unwrap();
    create(&db, &root, "目标").unwrap();
    let 子 = id_of(&db, "父/子");
    let 目标 = id_of(&db, "目标");
    move_to(&db, &root, 子, 目标).unwrap();
    let entries = history::list(&db).unwrap();
    assert!(entries[0].label.contains("父/子"));
    trash(&db, &root, id_of(&db, "目标/子")).unwrap();
    let entries = history::list(&db).unwrap();
    assert_eq!(entries[0].kind, HistoryKind::FolderTrash);
    assert!(entries[0].label.contains("目标/子"));
}

#[test]
fn undo_multi_folder_move_restores_every_directory_on_failure() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "甲").unwrap();
    create(&db, &root, "乙").unwrap();
    create(&db, &root, "目标").unwrap();
    let destination = id_of(&db, "目标");
    place_many(
        &db,
        &root,
        &[id_of(&db, "甲"), id_of(&db, "乙")],
        destination,
        None,
    )
    .unwrap();
    assert!(root.join("目标").join("甲").is_dir());
    assert!(root.join("目标").join("乙").is_dir());

    // 第二项在磁盘上消失：撤销会在处理它时报错，第一项已经搬回原位置。
    fs::remove_dir_all(root.join("目标").join("乙")).unwrap();

    let target = history::list(&db).unwrap().first().unwrap().id;
    let thumbs = crate::library::thumbs::ThumbPack::open(LibraryPaths::new(&root).thumbs).unwrap();
    let task = crate::app::tasks::TaskHandle::new(
        "folder-test-undo",
        crate::app::tasks::TaskKind::Undo,
        "测试",
    );
    assert!(history::run_undo(&db, &root, &thumbs, target, task).is_err());

    // 整次当作没发生：数据库仍是移动后的路径，磁盘也必须回到移动后的位置。
    let moved = id_of(&db, "目标/甲");
    assert_eq!(
        db.with_reader(|connection| folder_path(connection, moved))
            .unwrap(),
        "目标/甲"
    );
    assert!(root.join("目标").join("甲").is_dir());
    assert!(!root.join("甲").exists());
}

#[test]
fn move_guard_is_case_insensitive_for_legacy_paths() {
    let (_temp, db, root) = fixture();
    create(&db, &root, "Art").unwrap();
    let art = id_of(&db, "Art");
    // 旧数据可能把子路径写成另一种大小写；这里直接造出这种行。
    let child = db
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO folders(path, created_at, sort_index) VALUES ('art/sub', 1, 0)",
                [],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .unwrap();
    fs::create_dir_all(root.join("Art").join("sub")).unwrap();

    let error = place_many(&db, &root, &[art], child, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("不能把文件夹移动到自身或子文件夹"),
        "{error}"
    );
    assert!(move_to(&db, &root, art, child).is_err());
}

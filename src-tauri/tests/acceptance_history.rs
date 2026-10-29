#![cfg(feature = "headless")]

mod acceptance_support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use acceptance_support::{
    Report, Tier, Workspace, create_library, headless, history_entries, image_ids, library_counts,
    skip_unless,
};

#[test]
fn undo_and_redo_restore_trash_and_move_facts() {
    if skip_unless(Tier::Cheap, "undo_and_redo_restore_trash_and_move_facts") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "undo_and_redo_restore_trash_and_move_facts",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report);
    let ids = image_ids(&workspace, &library);

    let trashed = headless(&workspace, "metadata")
        .action("trash")
        .library(&library)
        .request(json!({ "imageIds": ids[0..2] }))
        .finish();
    report.record(&trashed);
    trashed.expect_ok();
    assert_eq!(
        library_counts(&workspace, &library, &mut report)["trashCount"],
        2
    );

    let trash_entry = entry(&workspace, &library, "trash");
    let undone = headless(&workspace, "history")
        .action("undo")
        .library(&library)
        .request(json!({ "targetId": trash_entry["id"] }))
        .finish();
    report.record(&undone);
    undone.expect_ok();
    assert_eq!(undone.data()["task"]["phase"], "completed");
    assert_eq!(
        library_counts(&workspace, &library, &mut report)["trashCount"],
        0
    );
    let entry_after = history_entry(&workspace, &library, trash_entry["id"].as_i64().unwrap());
    assert_eq!(entry_after["status"], "undone");

    let redone = headless(&workspace, "history")
        .action("redo")
        .library(&library)
        .request(json!({ "targetId": trash_entry["id"] }))
        .finish();
    report.record(&redone);
    redone.expect_ok();
    assert_eq!(
        library_counts(&workspace, &library, &mut report)["trashCount"],
        2
    );
    let entry_after = history_entry(&workspace, &library, trash_entry["id"].as_i64().unwrap());
    assert_eq!(entry_after["status"], "applied");

    let restored = headless(&workspace, "metadata")
        .action("restore")
        .library(&library)
        .request(json!({ "imageIds": ids[0..2] }))
        .finish();
    report.record(&restored);
    restored.expect_ok();

    let original_paths: Vec<PathBuf> = ids[2..4]
        .iter()
        .map(|id| image_file(&workspace, &library, &mut report, *id))
        .collect();
    let folder = headless(&workspace, "folder")
        .action("create")
        .library(&library)
        .request(json!({ "path": "移动目标" }))
        .finish();
    report.record(&folder);
    folder.expect_ok();

    let moved = headless(&workspace, "metadata")
        .action("move")
        .library(&library)
        .request(json!({ "imageIds": ids[2..4], "folderPath": "移动目标" }))
        .finish();
    report.record(&moved);
    moved.expect_ok();
    assert_eq!(moved.data()["moved"], 2);
    for id in &ids[2..4] {
        let path = image_file(&workspace, &library, &mut report, *id);
        assert!(path.is_file(), "移动后文件必须存在：{}", path.display());
        assert!(
            path.to_string_lossy().contains("移动目标"),
            "{}",
            path.display()
        );
    }

    let move_entry = entry(&workspace, &library, "move");
    let undone = headless(&workspace, "history")
        .action("undo")
        .library(&library)
        .request(json!({ "targetId": move_entry["id"] }))
        .finish();
    report.record(&undone);
    undone.expect_ok();
    for (index, id) in ids[2..4].iter().enumerate() {
        let path = image_file(&workspace, &library, &mut report, *id);
        assert!(
            path.is_file(),
            "撤销移动后文件必须回到原位：{}",
            path.display()
        );
        assert_eq!(path, original_paths[index]);
    }

    let clear = headless(&workspace, "history")
        .action("clear")
        .library(&library)
        .finish();
    report.record(&clear);
    clear.expect_ok();
    assert!(history_entries(&workspace, &library).is_empty());

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn undo_and_redo_survive_library_reopen() {
    if skip_unless(Tier::Cheap, "undo_and_redo_survive_library_reopen") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "undo_and_redo_survive_library_reopen",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report);
    let ids = image_ids(&workspace, &library);

    let trashed = headless(&workspace, "metadata")
        .action("trash")
        .library(&library)
        .request(json!({ "imageIds": [ids[0]] }))
        .finish();
    report.record(&trashed);
    trashed.expect_ok();
    let trash_entry = entry(&workspace, &library, "trash");
    let undone = headless(&workspace, "history")
        .action("undo")
        .library(&library)
        .request(json!({ "targetId": trash_entry["id"] }))
        .finish();
    report.record(&undone);
    undone.expect_ok();

    let closed = headless(&workspace, "library")
        .action("close")
        .library(&library)
        .finish();
    report.record(&closed);
    closed.expect_ok();

    let redone = headless(&workspace, "history")
        .action("redo")
        .library(&library)
        .request(json!({ "targetId": trash_entry["id"] }))
        .finish();
    report.record(&redone);
    redone.expect_ok();
    assert_eq!(
        library_counts(&workspace, &library, &mut report)["trashCount"],
        1
    );

    workspace.assert_within_budget();
    report.success();
}

fn seed(workspace: &Workspace, library: &Path, report: &mut Report) {
    let outcome = headless(workspace, "fixture")
        .action("seed-db")
        .library(library)
        .request(json!({
            "folders": ["原目录"],
            "images": 6,
            "tagsPerImage": 0,
            "extensions": ["jpg"],
            "writeFiles": true,
        }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
}

fn entry(workspace: &Workspace, library: &Path, kind: &str) -> Value {
    history_entries(workspace, library)
        .into_iter()
        .find(|entry| entry["kind"] == kind)
        .unwrap_or_else(|| panic!("没有 {kind} 记录"))
}

fn history_entry(workspace: &Workspace, library: &Path, id: i64) -> Value {
    history_entries(workspace, library)
        .into_iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("没有 id={id} 的记录"))
}

fn image_file(workspace: &Workspace, library: &Path, report: &mut Report, id: i64) -> PathBuf {
    let outcome = headless(workspace, "query")
        .action("detail")
        .library(library)
        .request(json!({ "imageId": id }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    PathBuf::from(outcome.data()["filePath"].as_str().expect("filePath"))
}

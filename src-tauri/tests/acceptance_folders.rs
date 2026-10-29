#![cfg(feature = "headless")]

mod acceptance_support;

use std::path::Path;

use serde_json::{Value, json};

use acceptance_support::{
    Report, Tier, Workspace, create_library, folder_id, headless, history_entries, skip_unless,
};

#[test]
fn folder_create_rename_move_and_reorder() {
    if skip_unless(Tier::Cheap, "folder_create_rename_move_and_reorder") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "folder_create_rename_move_and_reorder",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report);

    let created = headless(&workspace, "folder")
        .action("create")
        .library(&library)
        .request(json!({ "path": "道具" }))
        .finish();
    report.record(&created);
    created.expect_ok();
    assert!(
        created.data()["folders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|folder| folder["path"] == "道具")
    );

    let conflict = headless(&workspace, "folder")
        .action("create")
        .library(&library)
        .request(json!({ "path": "场景" }))
        .finish();
    report.record(&conflict);
    conflict.expect_failed("文件夹已存在");
    let paths = folder_paths(&workspace, &library, &mut report);
    assert_eq!(paths.iter().filter(|path| *path == "场景").count(), 1);

    let prop = folder_id(&workspace, &library, &mut report, "道具");
    let renamed = headless(&workspace, "folder")
        .action("rename")
        .library(&library)
        .request(json!({ "folderId": prop, "name": "道具组" }))
        .finish();
    report.record(&renamed);
    renamed.expect_ok();
    let paths = folder_paths(&workspace, &library, &mut report);
    assert!(paths.contains(&"道具组".to_owned()), "{paths:?}");
    assert!(!paths.contains(&"道具".to_owned()));

    let scene = folder_id(&workspace, &library, &mut report, "场景");
    let moved = headless(&workspace, "folder")
        .action("move")
        .library(&library)
        .request(json!({ "folderId": prop, "parentId": scene }))
        .finish();
    report.record(&moved);
    moved.expect_ok();
    let paths = folder_paths(&workspace, &library, &mut report);
    assert!(paths.contains(&"场景/道具组".to_owned()), "{paths:?}");

    let root = folder_id(&workspace, &library, &mut report, "");
    let placed = headless(&workspace, "folder")
        .action("place")
        .library(&library)
        .request(json!({ "folderIds": [prop], "parentId": root, "sortIndex": 0 }))
        .finish();
    report.record(&placed);
    placed.expect_ok();
    let paths = folder_paths(&workspace, &library, &mut report);
    assert!(paths.contains(&"道具组".to_owned()), "{paths:?}");

    let root_children: Vec<i64> = folder_rows(&workspace, &library, &mut report)
        .iter()
        .filter(|folder| {
            let path = folder["path"].as_str().unwrap();
            !path.is_empty() && !path.contains('/')
        })
        .map(|folder| folder["id"].as_i64().unwrap())
        .collect();
    let ordered = headless(&workspace, "folder")
        .action("apply-order")
        .library(&library)
        .request(json!({
            "folderId": root,
            "key": "name",
            "descending": false,
            "recursive": false,
            "orders": [{ "parentId": root, "folderIds": root_children }],
        }))
        .finish();
    report.record(&ordered);
    ordered.expect_ok();

    let reordered = headless(&workspace, "folder")
        .action("reorder")
        .library(&library)
        .request(json!({ "folderIds": [scene], "parentId": root, "sortIndex": 0 }))
        .finish();
    report.record(&reordered);
    reordered.expect_ok();

    let history = history_entries(&workspace, &library);
    for kind in [
        "folderCreate",
        "folderRename",
        "folderMove",
        "folderReorder",
    ] {
        assert!(
            history.iter().any(|entry| entry["kind"] == kind),
            "缺少 {kind}：{history:?}"
        );
    }

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn folder_trash_restore_and_permanent_delete() {
    if skip_unless(Tier::Cheap, "folder_trash_restore_and_permanent_delete") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "folder_trash_restore_and_permanent_delete",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report);
    let scene = folder_id(&workspace, &library, &mut report, "场景");

    let trashed = headless(&workspace, "folder")
        .action("delete")
        .library(&library)
        .request(json!({ "folderId": scene }))
        .finish();
    report.record(&trashed);
    trashed.expect_ok();
    let paths = folder_paths(&workspace, &library, &mut report);
    assert!(!paths.contains(&"场景".to_owned()), "{paths:?}");

    let trash_tree = headless(&workspace, "folder")
        .action("trash")
        .library(&library)
        .finish();
    report.record(&trash_tree);
    trash_tree.expect_ok();
    assert!(
        trash_tree.data()["folders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|folder| folder["path"] == "场景"),
        "{trash_tree:?}"
    );

    let restored = headless(&workspace, "folder")
        .action("restore")
        .library(&library)
        .request(json!({ "folderId": scene }))
        .finish();
    report.record(&restored);
    restored.expect_ok();
    assert!(
        folder_paths(&workspace, &library, &mut report).contains(&"场景".to_owned()),
        "恢复后文件夹应回到原路径"
    );

    let trashed_again = headless(&workspace, "folder")
        .action("delete")
        .library(&library)
        .request(json!({ "folderId": scene }))
        .finish();
    report.record(&trashed_again);
    trashed_again.expect_ok();
    let purged = headless(&workspace, "folder")
        .action("delete-permanently")
        .library(&library)
        .request(json!({ "folderId": scene }))
        .finish();
    report.record(&purged);
    purged.expect_ok();

    let cleared = headless(&workspace, "history")
        .action("clear")
        .library(&library)
        .finish();
    report.record(&cleared);
    cleared.expect_ok();
    let paths = folder_paths(&workspace, &library, &mut report);
    assert!(!paths.contains(&"场景".to_owned()), "{paths:?}");

    workspace.assert_within_budget();
    report.success();
}

fn seed(workspace: &Workspace, library: &Path, report: &mut Report) {
    let outcome = headless(workspace, "fixture")
        .action("seed-db")
        .library(library)
        .request(json!({
            "folders": ["人物/速写", "场景"],
            "images": 6,
            "tagsPerImage": 0,
            "extensions": ["png"],
            "writeFiles": true,
        }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
}

fn folder_rows(workspace: &Workspace, library: &Path, report: &mut Report) -> Vec<Value> {
    let outcome = headless(workspace, "query")
        .action("folders")
        .library(library)
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    outcome.data().as_array().cloned().unwrap_or_default()
}

fn folder_paths(workspace: &Workspace, library: &Path, report: &mut Report) -> Vec<String> {
    folder_rows(workspace, library, report)
        .into_iter()
        .map(|folder| folder["path"].as_str().unwrap().to_owned())
        .collect()
}

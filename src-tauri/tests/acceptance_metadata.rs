#![cfg(feature = "headless")]

mod acceptance_support;

use std::path::Path;

use serde_json::json;

use acceptance_support::{
    Report, Tier, Workspace, create_library, headless, history_entries, image_ids, library_counts,
    skip_unless,
};

#[test]
fn metadata_fields_and_tags_round_trip() {
    if skip_unless(Tier::Cheap, "metadata_fields_and_tags_round_trip") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "metadata_fields_and_tags_round_trip",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report);

    let ids = image_ids(&workspace, &library);
    assert_eq!(ids.len(), 12);
    let first = ids[0];
    let second = ids[1];

    let scored = headless(&workspace, "metadata")
        .action("score")
        .library(&library)
        .request(json!({ "imageId": first, "score": 5 }))
        .finish();
    report.record(&scored);
    scored.expect_ok();

    let described = headless(&workspace, "metadata")
        .action("description")
        .library(&library)
        .request(json!({ "imageId": first, "description": "验收描述" }))
        .finish();
    report.record(&described);
    described.expect_ok();

    let tagged = headless(&workspace, "metadata")
        .action("tags")
        .library(&library)
        .request(json!({ "imageId": first, "tagNames": ["甲", "乙"] }))
        .finish();
    report.record(&tagged);
    tagged.expect_ok();

    let added = headless(&workspace, "metadata")
        .action("add-tags")
        .library(&library)
        .request(json!({ "imageIds": [first, second], "tagNames": ["丙"] }))
        .finish();
    report.record(&added);
    added.expect_ok();
    assert_eq!(added.data(), 2, "返回受影响图片数");

    let detail = headless(&workspace, "query")
        .action("detail")
        .library(&library)
        .request(json!({ "imageId": first }))
        .finish();
    report.record(&detail);
    detail.expect_ok();
    assert_eq!(detail.data()["score"], 5);
    assert_eq!(detail.data()["description"], "验收描述");
    let tags: Vec<&str> = detail.data()["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag["name"].as_str().unwrap())
        .collect();
    assert_eq!(tags, vec!["丙", "乙", "甲"]);

    let selection = headless(&workspace, "query")
        .action("selection-tags")
        .library(&library)
        .request(json!({ "imageIds": [first, second] }))
        .finish();
    report.record(&selection);
    selection.expect_ok();
    assert!(
        selection.data()["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tag| tag["name"] == "丙"),
        "{selection:?}"
    );

    let removed = headless(&workspace, "metadata")
        .action("remove-tags")
        .library(&library)
        .request(json!({ "imageIds": [first], "tagNames": ["甲"] }))
        .finish();
    report.record(&removed);
    removed.expect_ok();
    assert_eq!(removed.data(), 1);

    let tags_page = headless(&workspace, "query")
        .action("tags")
        .library(&library)
        .request(json!({ "includeEmpty": true }))
        .finish();
    report.record(&tags_page);
    tags_page.expect_ok();
    let tag_rows = tags_page.data().as_array().unwrap();
    let tag_id = tag_rows
        .iter()
        .find(|tag| tag["name"] == "乙")
        .expect("乙 标签")["id"]
        .as_i64()
        .unwrap();

    let renamed = headless(&workspace, "metadata")
        .action("rename-tag")
        .library(&library)
        .request(json!({ "tagId": tag_id, "name": "乙改" }))
        .finish();
    report.record(&renamed);
    renamed.expect_ok();

    let merged = headless(&workspace, "metadata")
        .action("merge-tags")
        .library(&library)
        .request(json!({ "sourceId": tag_id, "targetId": tag_rows[0]["id"] }))
        .finish();
    report.record(&merged);
    merged.expect_ok();

    let history = history_entries(&workspace, &library);
    for kind in [
        "score",
        "description",
        "tagSet",
        "tagAdd",
        "tagRemove",
        "tagRename",
        "tagMerge",
    ] {
        assert!(
            history.iter().any(|entry| entry["kind"] == kind),
            "缺少 {kind} 记录：{history:?}"
        );
    }

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn collections_lifecycle_and_membership() {
    if skip_unless(Tier::Cheap, "collections_lifecycle_and_membership") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "collections_lifecycle_and_membership",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report);
    let ids = image_ids(&workspace, &library);

    let first = headless(&workspace, "collection")
        .action("create")
        .library(&library)
        .request(json!({ "name": "集合甲" }))
        .finish();
    report.record(&first);
    first.expect_ok();
    let first_id = first.data()["id"].as_i64().unwrap();

    let second = headless(&workspace, "collection")
        .action("create")
        .library(&library)
        .request(json!({ "name": "集合乙" }))
        .finish();
    report.record(&second);
    second.expect_ok();
    let second_id = second.data()["id"].as_i64().unwrap();

    add_to_collection(&workspace, &library, &mut report, first_id, &ids[0..4]);
    add_to_collection(&workspace, &library, &mut report, second_id, &ids[2..6]);

    let listed = headless(&workspace, "collection")
        .action("list")
        .library(&library)
        .finish();
    report.record(&listed);
    listed.expect_ok();
    let rows = listed.data().as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let first_row = rows.iter().find(|row| row["id"] == first_id).unwrap();
    assert_eq!(first_row["imageCount"], 4);

    let combined = headless(&workspace, "collection")
        .action("combine")
        .library(&library)
        .request(json!({
            "mode": "union",
            "collectionIds": [first_id, second_id],
            "name": "并集",
        }))
        .finish();
    report.record(&combined);
    combined.expect_ok();
    let combined_id = combined.data()["id"].as_i64().unwrap();
    assert_eq!(combined.data()["imageCount"], 6);

    let removed = headless(&workspace, "collection")
        .action("remove")
        .library(&library)
        .request(json!({ "collectionId": combined_id, "imageIds": [ids[0], ids[1]] }))
        .finish();
    report.record(&removed);
    removed.expect_ok();
    assert_eq!(removed.data(), 2);

    let renamed = headless(&workspace, "collection")
        .action("rename")
        .library(&library)
        .request(json!({ "collectionId": first_id, "name": "改名后" }))
        .finish();
    report.record(&renamed);
    renamed.expect_ok();

    let reordered = headless(&workspace, "collection")
        .action("reorder")
        .library(&library)
        .request(json!({ "collectionIds": [combined_id, second_id, first_id] }))
        .finish();
    report.record(&reordered);
    reordered.expect_ok();

    let deleted = headless(&workspace, "collection")
        .action("delete")
        .library(&library)
        .request(json!({ "collectionId": second_id }))
        .finish();
    report.record(&deleted);
    deleted.expect_ok();

    let listed = headless(&workspace, "collection")
        .action("list")
        .library(&library)
        .finish();
    report.record(&listed);
    listed.expect_ok();
    assert_eq!(listed.data().as_array().unwrap().len(), 2);

    let history = history_entries(&workspace, &library);
    for kind in ["collectionCreate", "collectionRename", "collectionDelete"] {
        assert!(
            history.iter().any(|entry| entry["kind"] == kind),
            "缺少 {kind} 记录：{history:?}"
        );
    }

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn trash_restore_and_permanent_delete_are_deferred_until_history_clear() {
    if skip_unless(
        Tier::Cheap,
        "trash_restore_and_permanent_delete_are_deferred_until_history_clear",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "trash_restore_and_permanent_delete_are_deferred_until_history_clear",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report);
    let ids = image_ids(&workspace, &library);
    let files = image_files(&workspace, &library, &mut report, &ids[0..3]);
    for file in &files {
        assert!(file.is_file(), "{}", file.display());
    }

    let trashed = headless(&workspace, "metadata")
        .action("trash")
        .library(&library)
        .request(json!({ "imageIds": ids[0..3] }))
        .finish();
    report.record(&trashed);
    trashed.expect_ok();
    assert_eq!(trashed.data(), 3);
    let now_counts = library_counts(&workspace, &library, &mut report);
    assert_eq!(now_counts["trashCount"], 3);
    assert_eq!(now_counts["imageCount"], 9);

    let restored = headless(&workspace, "metadata")
        .action("restore")
        .library(&library)
        .request(json!({ "imageIds": ids[0..3] }))
        .finish();
    report.record(&restored);
    restored.expect_ok();
    assert_eq!(restored.data(), 3);
    let now_counts = library_counts(&workspace, &library, &mut report);
    assert_eq!(now_counts["trashCount"], 0);

    let trashed = headless(&workspace, "metadata")
        .action("trash")
        .library(&library)
        .request(json!({ "imageIds": ids[0..2] }))
        .finish();
    report.record(&trashed);
    trashed.expect_ok();

    let deleted = headless(&workspace, "metadata")
        .action("delete")
        .library(&library)
        .request(json!({ "imageIds": ids[0..2] }))
        .finish();
    report.record(&deleted);
    deleted.expect_ok();
    assert_eq!(deleted.data()["deleted"], 2);
    assert_eq!(deleted.data()["deferred"], true);
    for file in &files[0..2] {
        assert!(file.is_file(), "待释放期间原图必须保留：{}", file.display());
    }

    let cleared = headless(&workspace, "history")
        .action("clear")
        .library(&library)
        .finish();
    report.record(&cleared);
    cleared.expect_ok();
    for file in &files[0..2] {
        assert!(
            file.is_file(),
            "清空记录本身不释放文件，待删原图应保留：{}",
            file.display()
        );
    }
    let released = headless(&workspace, "history")
        .action("release")
        .library(&library)
        .finish();
    report.record(&released);
    released.expect_ok();
    for file in &files[0..2] {
        assert!(!file.exists(), "释放待删文件应删除原图：{}", file.display());
    }
    let now_counts = library_counts(&workspace, &library, &mut report);
    assert_eq!(now_counts["imageCount"], 10);
    assert_eq!(now_counts["trashCount"], 0);

    let history = history_entries(&workspace, &library);
    assert!(history.is_empty(), "{history:?}");

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn heavy_move_20k_images_and_files() {
    if skip_unless(Tier::Heavy, "heavy_move_20k_images_and_files") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new("heavy_move_20k_images_and_files", Tier::Heavy, &workspace);
    let (library, _) = create_library(&workspace);
    let seeded = headless(&workspace, "fixture")
        .action("seed-db")
        .library(&library)
        .request(json!({
            "folders": ["源", "目标"],
            "imageFolders": ["源"],
            "images": 20_000,
            "tagsPerImage": 0,
            "extensions": ["jpg"],
            "writeFiles": true,
        }))
        .timeout(std::time::Duration::from_secs(180))
        .finish();
    report.record(&seeded);
    seeded.expect_ok();
    assert_eq!(seeded.data()["files"], 20_000);

    let ids = image_ids(&workspace, &library);
    assert_eq!(ids.len(), 20_000);
    let moved = headless(&workspace, "metadata")
        .action("move")
        .library(&library)
        .request(json!({ "imageIds": ids, "folderPath": "目标" }))
        .timeout(std::time::Duration::from_secs(180))
        .finish();
    report.record(&moved);
    moved.expect_ok();
    assert_eq!(moved.data()["moved"], 20_000);
    assert_eq!(moved.data()["failures"].as_array().unwrap().len(), 0);
    let target_files = std::fs::read_dir(library.join("目标")).unwrap().count();
    assert_eq!(target_files, 20_000);
    assert!(
        history_entries(&workspace, &library)
            .iter()
            .any(|entry| entry["kind"] == "move" && entry["affected"] == 20_000)
    );

    workspace.assert_within_budget();
    report.success();
}

fn seed(workspace: &Workspace, library: &Path, report: &mut Report) {
    let outcome = headless(workspace, "fixture")
        .action("seed-db")
        .library(library)
        .request(json!({
            "folders": ["人物", "场景"],
            "tags": ["红", "蓝"],
            "images": 12,
            "tagsPerImage": 1,
            "extensions": ["jpg"],
            "writeFiles": true,
        }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    assert_eq!(outcome.data()["images"], 12);
}

fn image_files(
    workspace: &Workspace,
    library: &Path,
    report: &mut Report,
    ids: &[i64],
) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for id in ids {
        let detail = headless(workspace, "query")
            .action("detail")
            .library(library)
            .request(json!({ "imageId": id }))
            .finish();
        report.record(&detail);
        detail.expect_ok();
        files.push(std::path::PathBuf::from(
            detail.data()["filePath"].as_str().unwrap(),
        ));
    }
    files
}

fn add_to_collection(
    workspace: &Workspace,
    library: &Path,
    report: &mut Report,
    collection_id: i64,
    ids: &[i64],
) {
    let outcome = headless(workspace, "collection")
        .action("add")
        .library(library)
        .request(json!({ "collectionId": collection_id, "imageIds": ids }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    assert_eq!(outcome.data(), ids.len());
}

#![cfg(feature = "headless")]

mod acceptance_support;

use std::{fs, path::Path};

use serde_json::{Value, json};

use acceptance_support::{
    Report, Tier, Workspace, create_library, headless, history_entries, import_images, skip_unless,
};

#[test]
fn viewer_source_reports_format_and_preview_mode() {
    if skip_unless(Tier::Cheap, "viewer_source_reports_format_and_preview_mode") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "viewer_source_reports_format_and_preview_mode",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let ids = import_images(&workspace, &library, &mut report, 2, "编辑");
    let id = ids[0];

    let source = headless(&workspace, "viewer")
        .action("source")
        .library(&library)
        .request(json!({ "imageId": id }))
        .finish();
    report.record(&source);
    source.expect_ok();
    let data = source.data();
    assert_eq!(data["format"], "png");
    assert_eq!(data["width"], 32);
    assert_eq!(data["height"], 24);
    assert_eq!(data["displayWidth"], 32);
    assert_eq!(data["displayHeight"], 24);
    assert_eq!(data["preview"], false);
    assert_eq!(data["editable"], true);
    assert_eq!(data["deleted"], false);

    let detail = detail(&workspace, &library, &mut report, id);
    fs::remove_file(detail["filePath"].as_str().unwrap()).unwrap();
    let missing = headless(&workspace, "viewer")
        .action("source")
        .library(&library)
        .request(json!({ "imageId": id }))
        .finish();
    report.record(&missing);
    missing.expect_failed("原图文件不存在");

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn edit_estimate_save_and_failure_keeps_metadata_and_history() {
    if skip_unless(
        Tier::Cheap,
        "edit_estimate_save_and_failure_keeps_metadata_and_history",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "edit_estimate_save_and_failure_keeps_metadata_and_history",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let ids = import_images(&workspace, &library, &mut report, 2, "编辑");
    let id = ids[0];

    headless(&workspace, "metadata")
        .action("score")
        .library(&library)
        .request(json!({ "imageId": id, "score": 4 }))
        .finish()
        .expect_ok();
    headless(&workspace, "metadata")
        .action("add-tags")
        .library(&library)
        .request(json!({ "imageIds": [id], "tagNames": ["编辑"] }))
        .finish()
        .expect_ok();

    let estimate = headless(&workspace, "edit")
        .action("estimate")
        .library(&library)
        .request(json!({
            "imageId": id,
            "resize": { "width": 16, "height": 12 },
        }))
        .finish();
    report.record(&estimate);
    estimate.expect_ok();
    assert!(estimate.data()["fileSize"].as_i64().unwrap() > 0);
    assert_eq!(estimate.data()["exact"], true);

    let before = detail(&workspace, &library, &mut report, id);
    let saved = headless(&workspace, "edit")
        .action("save")
        .library(&library)
        .request(json!({
            "imageId": id,
            "resize": { "width": 16, "height": 12 },
        }))
        .finish();
    report.record(&saved);
    saved.expect_ok();
    assert_eq!(saved.data()["width"], 16);
    assert_eq!(saved.data()["height"], 12);
    assert_eq!(saved.data()["changed"], true);
    assert_eq!(saved.data()["oldFileRemoved"], true);
    assert_ne!(saved.data()["contentHash"], before["contentHash"]);

    let after = detail(&workspace, &library, &mut report, id);
    assert_eq!(after["width"], 16);
    assert_eq!(after["height"], 12);
    assert_eq!(after["score"], 4);
    assert!(
        after["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tag| tag["name"] == "编辑"),
        "{after:?}"
    );
    let file = std::path::PathBuf::from(after["filePath"].as_str().unwrap());
    assert!(file.is_file(), "{}", file.display());

    let history = history_entries(&workspace, &library);
    assert!(
        history.iter().any(|entry| entry["kind"] == "edit"),
        "{history:?}"
    );

    let too_large = headless(&workspace, "edit")
        .action("save")
        .library(&library)
        .request(json!({
            "imageId": id,
            "resize": { "width": 10000, "height": 10000 },
        }))
        .finish();
    report.record(&too_large);
    too_large.expect_failed("超过编辑上限");

    fs::remove_file(&file).unwrap();
    let missing = headless(&workspace, "edit")
        .action("save")
        .library(&library)
        .request(json!({ "imageId": id, "rotate": 1 }))
        .finish();
    report.record(&missing);
    missing.expect_failed("无法打开图片");
    let after = detail(&workspace, &library, &mut report, id);
    assert_eq!(after["contentHash"], saved.data()["contentHash"]);
    assert_eq!(after["score"], 4);

    workspace.assert_within_budget();
    report.success();
}

fn detail(workspace: &Workspace, library: &Path, report: &mut Report, id: i64) -> Value {
    let outcome = headless(workspace, "query")
        .action("detail")
        .library(library)
        .request(json!({ "imageId": id }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    outcome.data().clone()
}

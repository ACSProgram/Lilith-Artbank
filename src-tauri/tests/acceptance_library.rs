#![cfg(feature = "headless")]

mod acceptance_support;

use std::{fs, path::Path};

use serde_json::json;

use acceptance_support::{Report, Tier, Workspace, headless, skip_unless};

#[test]
fn library_lifecycle_create_inspect_open_close() {
    if skip_unless(Tier::Cheap, "library_lifecycle_create_inspect_open_close") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "library_lifecycle_create_inspect_open_close",
        Tier::Cheap,
        &workspace,
    );
    let library = workspace.library();

    let empty = headless(&workspace, "library")
        .action("inspect-path")
        .request(json!({ "path": library.to_string_lossy() }))
        .finish();
    report.record(&empty);
    empty.expect_ok();
    assert_eq!(empty.data()["kind"], "empty");

    let created = headless(&workspace, "library")
        .action("create")
        .request(json!({
            "path": library.to_string_lossy(),
            "name": "验收库",
        }))
        .finish();
    report.record(&created);
    created.expect_ok();
    let library_id = created.data()["id"].as_str().unwrap().to_owned();
    assert_eq!(created.data()["name"], "验收库");
    assert_eq!(created.data()["imageCount"], 0);
    assert!(created.data()["schemaVersion"].as_u64().unwrap() >= 9);
    assert!(library.join(".artbank").join("library.db").is_file());

    let inspected = headless(&workspace, "library")
        .action("inspect-path")
        .request(json!({ "path": library.to_string_lossy() }))
        .finish();
    report.record(&inspected);
    inspected.expect_ok();
    assert_eq!(inspected.data()["kind"], "library");
    assert_eq!(inspected.data()["manifest"]["libraryId"], library_id);

    let opened = headless(&workspace, "library")
        .action("open")
        .request(json!({ "path": library.to_string_lossy() }))
        .finish();
    report.record(&opened);
    opened.expect_ok();
    assert_eq!(opened.data()["id"], library_id);

    let info = headless(&workspace, "library")
        .action("info")
        .library(&library)
        .finish();
    report.record(&info);
    info.expect_ok();
    assert_eq!(info.data()["id"], library_id);

    let counts = headless(&workspace, "library")
        .action("counts")
        .library(&library)
        .finish();
    report.record(&counts);
    counts.expect_ok();
    assert_eq!(counts.data()["imageCount"], 0);
    assert_eq!(counts.data()["trashCount"], 0);
    assert_eq!(counts.data()["tempCount"], 0);

    let closed = headless(&workspace, "library")
        .action("close")
        .library(&library)
        .finish();
    report.record(&closed);
    closed.expect_ok();
    assert_eq!(closed.data()["closed"], true);

    let settings = acceptance_support::read_settings(&workspace);
    assert!(settings["libraryPath"].is_null(), "{settings}");

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn opening_plain_directory_fails_and_logs_error() {
    if skip_unless(Tier::Cheap, "opening_plain_directory_fails_and_logs_error") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "opening_plain_directory_fails_and_logs_error",
        Tier::Cheap,
        &workspace,
    );
    let plain = workspace.out("plain");
    fs::create_dir_all(&plain).unwrap();

    let failed = headless(&workspace, "library")
        .action("open")
        .request(json!({ "path": plain.to_string_lossy() }))
        .finish();
    report.record(&failed);
    failed.expect_failed("目录不是图库");
    assert_eq!(
        failed.stdout.trim().lines().count(),
        1,
        "stdout 必须只有一行 JSON"
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn open_configured_round_trip() {
    if skip_unless(Tier::Cheap, "open_configured_round_trip") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new("open_configured_round_trip", Tier::Cheap, &workspace);
    let library = workspace.library();

    let none = headless(&workspace, "library")
        .action("open-configured")
        .finish();
    report.record(&none);
    none.expect_ok();
    assert_eq!(none.data()["configured"], false);

    let created = headless(&workspace, "library")
        .action("create")
        .request(json!({ "path": library.to_string_lossy(), "name": "配置库" }))
        .finish();
    report.record(&created);
    created.expect_ok();
    let library_id = created.data()["id"].as_str().unwrap().to_owned();

    let opened = headless(&workspace, "library")
        .action("open")
        .request(json!({ "path": library.to_string_lossy() }))
        .finish();
    report.record(&opened);
    opened.expect_ok();
    assert_eq!(accepted_path(&workspace), library);

    let configured = headless(&workspace, "library")
        .action("open-configured")
        .finish();
    report.record(&configured);
    configured.expect_ok();
    assert_eq!(configured.data()["configured"], true);
    assert_eq!(configured.data()["library"]["id"], library_id);

    let closed = headless(&workspace, "library")
        .action("close")
        .library(&library)
        .finish();
    report.record(&closed);
    closed.expect_ok();

    let after_close = headless(&workspace, "library")
        .action("open-configured")
        .finish();
    report.record(&after_close);
    after_close.expect_ok();
    assert_eq!(after_close.data()["configured"], false);

    workspace.assert_within_budget();
    report.success();
}

fn accepted_path(workspace: &Workspace) -> std::path::PathBuf {
    let settings = acceptance_support::read_settings(workspace);
    std::path::PathBuf::from(settings["libraryPath"].as_str().unwrap())
}

#[test]
fn fixture_writes_small_images() {
    if skip_unless(Tier::Cheap, "fixture_writes_small_images") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new("fixture_writes_small_images", Tier::Cheap, &workspace);
    let image = workspace.out("single.png");

    let single = headless(&workspace, "fixture")
        .action("write-image")
        .request(json!({
            "path": image.to_string_lossy(),
            "width": 8,
            "height": 6,
            "format": "png",
            "transparent": true,
        }))
        .finish();
    report.record(&single);
    single.expect_ok();
    assert!(single.data()["bytes"].as_u64().unwrap() > 0);
    assert!(is_png(&image));

    let directory = workspace.out("corpus");
    let corpus = headless(&workspace, "fixture")
        .action("corpus")
        .request(json!({
            "directory": directory.to_string_lossy(),
            "count": 6,
            "formats": ["png", "jpg", "bmp"],
        }))
        .finish();
    report.record(&corpus);
    corpus.expect_ok();
    assert_eq!(corpus.data()["count"], 6);
    for index in 0..6 {
        let path = directory.join(corpus.data()["files"][index].as_str().expect("样本文件名"));
        assert!(path.is_file(), "{}", path.display());
        assert!(fs::metadata(&path).unwrap().len() > 0);
    }

    workspace.assert_within_budget();
    report.success();
}

fn is_png(path: &Path) -> bool {
    fs::read(path)
        .map(|bytes| bytes.starts_with(b"\x89PNG\r\n\x1a\n"))
        .unwrap_or(false)
}

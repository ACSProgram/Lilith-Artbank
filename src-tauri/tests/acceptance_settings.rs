#![cfg(feature = "headless")]

mod acceptance_support;

use serde_json::json;

use acceptance_support::{Report, Tier, Workspace, headless, read_settings, skip_unless};

#[test]
fn defaults_save_and_restart_round_trip() {
    if skip_unless(Tier::Cheap, "defaults_save_and_restart_round_trip") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "defaults_save_and_restart_round_trip",
        Tier::Cheap,
        &workspace,
    );

    let defaults = headless(&workspace, "settings").action("get").finish();
    report.record(&defaults);
    defaults.expect_ok();
    assert_eq!(defaults.data()["ui"]["theme"], "system");
    assert!(defaults.data()["libraryPath"].is_null());

    let mut settings = defaults.data().clone();
    settings["ui"]["theme"] = json!("dark");
    settings["performance"]["workerThreads"] = json!(3);
    settings["maintenance"]["autoCompactThumbs"] = json!(false);
    let saved = headless(&workspace, "settings")
        .action("save")
        .request(settings)
        .finish();
    report.record(&saved);
    saved.expect_ok();
    assert_eq!(saved.data()["ui"]["theme"], "dark");
    assert_eq!(saved.data()["performance"]["workerThreads"], 3);
    assert!(
        !workspace
            .settings_file()
            .with_extension("json.tmp")
            .exists()
    );

    let on_disk = read_settings(&workspace);
    assert_eq!(on_disk["ui"]["theme"], "dark");
    assert_eq!(on_disk["performance"]["workerThreads"], 3);
    assert_eq!(on_disk["maintenance"]["autoCompactThumbs"], false);

    let restarted = headless(&workspace, "settings").action("get").finish();
    report.record(&restarted);
    restarted.expect_ok();
    assert_eq!(restarted.data()["ui"]["theme"], "dark");
    assert_eq!(restarted.data()["performance"]["workerThreads"], 3);
    assert_eq!(restarted.data()["maintenance"]["autoCompactThumbs"], false);

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn library_path_is_preserved_and_cleared() {
    if skip_unless(Tier::Cheap, "library_path_is_preserved_and_cleared") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "library_path_is_preserved_and_cleared",
        Tier::Cheap,
        &workspace,
    );
    let library = workspace.library();

    let created = headless(&workspace, "library")
        .action("create")
        .request(json!({ "path": library.to_string_lossy(), "name": "设置库" }))
        .finish();
    report.record(&created);
    created.expect_ok();

    let opened = headless(&workspace, "library")
        .action("open")
        .request(json!({ "path": library.to_string_lossy() }))
        .finish();
    report.record(&opened);
    opened.expect_ok();

    let mut settings = headless(&workspace, "settings")
        .action("get")
        .finish()
        .expect_ok()
        .data()
        .clone();
    assert_eq!(
        settings["libraryPath"]
            .as_str()
            .map(std::path::PathBuf::from),
        Some(library.clone())
    );

    // 设置页提交的是整份 Settings；后端必须保留当前图库路径。
    settings["libraryPath"] = serde_json::Value::Null;
    settings["ui"]["theme"] = json!("light");
    let saved = headless(&workspace, "settings")
        .action("save")
        .request(settings)
        .finish();
    report.record(&saved);
    saved.expect_ok();
    assert_eq!(
        saved.data()["libraryPath"]
            .as_str()
            .map(std::path::PathBuf::from),
        Some(library.clone())
    );
    assert_eq!(read_settings(&workspace)["ui"]["theme"], "light");

    let closed = headless(&workspace, "library")
        .action("close")
        .library(&library)
        .finish();
    report.record(&closed);
    closed.expect_ok();
    assert!(read_settings(&workspace)["libraryPath"].is_null());

    workspace.assert_within_budget();
    report.success();
}

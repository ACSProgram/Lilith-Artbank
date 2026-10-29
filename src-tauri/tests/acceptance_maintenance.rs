#![cfg(feature = "headless")]

mod acceptance_support;

use std::{fs, path::Path};

use serde_json::{Value, json};

use acceptance_support::{
    Report, Tier, Workspace, create_library, headless, history_entries, import_images, skip_unless,
};

#[test]
fn check_finds_missing_orphan_and_mismatch_then_moves_orphans() {
    if skip_unless(
        Tier::Default,
        "check_finds_missing_orphan_and_mismatch_then_moves_orphans",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "check_finds_missing_orphan_and_mismatch_then_moves_orphans",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report, 8);
    let files = image_files(&workspace, &library);
    fs::remove_file(&files[0]).unwrap();
    fs::write(&files[1], [1_u8, 2]).unwrap();
    let orphan_name = format!("{}.png", "c".repeat(64));
    fs::write(library.join(&orphan_name), [9_u8; 5]).unwrap();
    let csv = workspace.out("report.csv");

    let scenario = headless(&workspace, "scenario")
        .action("maintenance_check")
        .library(&library)
        .request(json!({
            "mode": "full",
            "moveOrphans": true,
            "saveReportPath": csv.to_string_lossy(),
        }))
        .timeout(std::time::Duration::from_secs(60))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    let found = &data["report"];
    assert!(found["missingTotal"].as_u64().unwrap() >= 1, "{found}");
    assert!(found["orphanTotal"].as_u64().unwrap() >= 1, "{found}");
    assert!(found["sizeMismatchTotal"].as_u64().unwrap() >= 1, "{found}");
    assert!(found["hashMismatchTotal"].as_u64().unwrap() >= 1, "{found}");
    assert!(
        found["orphans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|orphan| orphan["path"] == orphan_name),
        "{found}"
    );
    assert_eq!(data["saved"], true, "{data}");
    assert_eq!(data["moveOrphans"]["phase"], "completed", "{data}");
    assert!(!library.join(&orphan_name).exists());
    assert!(
        library
            .join("recycle")
            .join("orphans")
            .join(&orphan_name)
            .is_file()
    );
    assert!(fs::read_to_string(&csv).unwrap().contains("missing"));

    let orphan_entry = history_entries(&workspace, &library)
        .into_iter()
        .find(|entry| entry["kind"] == "orphanMove")
        .expect("孤儿清理记录");
    let undone = headless(&workspace, "history")
        .action("undo")
        .library(&library)
        .request(json!({ "targetId": orphan_entry["id"] }))
        .finish();
    report.record(&undone);
    undone.expect_ok();
    assert!(
        library.join(&orphan_name).is_file(),
        "撤销孤儿清理后文件应回到原位"
    );

    let redone = headless(&workspace, "history")
        .action("redo")
        .library(&library)
        .request(json!({ "targetId": orphan_entry["id"] }))
        .finish();
    report.record(&redone);
    redone.expect_ok();
    assert!(!library.join(&orphan_name).exists());

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn export_copies_files_and_skips_existing_targets() {
    if skip_unless(
        Tier::Default,
        "export_copies_files_and_skips_existing_targets",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "export_copies_files_and_skips_existing_targets",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report, 4);
    let target = workspace.out("export");

    let exported = headless(&workspace, "maintenance")
        .action("export")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "targetDir": target.to_string_lossy(),
            "includeManifest": true,
        }))
        .finish();
    report.record(&exported);
    exported.expect_ok();
    assert_eq!(exported.data()["task"]["phase"], "completed");
    assert!(
        exported.data()["task"]["message"]
            .as_str()
            .unwrap()
            .contains("已导出 4 张"),
        "{exported:?}"
    );
    let mut files: Vec<String> = fs::read_dir(&target)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files.len(), 5, "4 张原图 + manifest.csv：{files:?}");
    assert!(files.iter().any(|name| name == "manifest.csv"));
    let manifest = fs::read_to_string(target.join("manifest.csv")).unwrap();
    assert_eq!(manifest.lines().count(), 5, "{manifest}");

    let again = headless(&workspace, "maintenance")
        .action("export")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "targetDir": target.to_string_lossy(),
            "includeManifest": true,
        }))
        .finish();
    report.record(&again);
    again.expect_ok();
    assert!(
        again.data()["task"]["message"]
            .as_str()
            .unwrap()
            .contains("跳过 4 张"),
        "{again:?}"
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn pack_stats_and_compact_run_without_garbage() {
    if skip_unless(Tier::Default, "pack_stats_and_compact_run_without_garbage") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "pack_stats_and_compact_run_without_garbage",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    import_images(&workspace, &library, &mut report, 4, "维护");

    let rebuilt = headless(&workspace, "maintenance")
        .action("thumbnails")
        .library(&library)
        .request(json!({ "overwrite": false }))
        .finish();
    report.record(&rebuilt);
    rebuilt.expect_ok();
    assert_eq!(rebuilt.data()["task"]["phase"], "completed");

    let pack = headless(&workspace, "maintenance")
        .action("pack")
        .library(&library)
        .finish();
    report.record(&pack);
    pack.expect_ok();
    assert!(
        pack.data()["stats"]["totalSize"].as_u64().unwrap() > 0,
        "{pack:?}"
    );
    assert!(
        !pack.data()["stats"]["segments"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{pack:?}"
    );
    assert!(pack.data()["stats"]["garbageBytes"].as_u64().unwrap() == 0);

    let compacted = headless(&workspace, "maintenance")
        .action("compact")
        .library(&library)
        .finish();
    report.record(&compacted);
    compacted.expect_ok();
    assert_eq!(compacted.data()["task"]["phase"], "completed");
    assert_eq!(compacted.data()["lastCompact"], Value::Null);

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn idle_gc_reclaims_pack_garbage_with_short_idle_window() {
    if skip_unless(
        Tier::Default,
        "idle_gc_reclaims_pack_garbage_with_short_idle_window",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "idle_gc_reclaims_pack_garbage_with_short_idle_window",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    import_images(&workspace, &library, &mut report, 6, "维护");

    let scenario = headless(&workspace, "scenario")
        .action("idle_gc")
        .library(&library)
        .request(json!({ "timeoutMs": 10000 }))
        .env("LILITH_IDLE_THRESHOLD_MS", "800")
        .env("LILITH_IDLE_POLL_MS", "150")
        .timeout(std::time::Duration::from_secs(45))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    let before = data["garbageBefore"].as_f64().unwrap();
    let after = data["garbageAfter"].as_f64().unwrap();
    assert!(before > 0.2, "清空记录后应产生可回收垃圾：{data}");
    assert!(
        data["lastCompact"].is_object(),
        "空闲回收必须留下落点：{data}"
    );
    assert!(after < before, "{data}");
    assert_eq!(data["idle"], true, "{data}");
    let counts = headless(&workspace, "library")
        .action("counts")
        .library(&library)
        .finish();
    report.record(&counts);
    counts.expect_ok();
    assert_eq!(counts.data()["imageCount"], 0);

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn idle_compaction_yields_when_activity_resumes() {
    if skip_unless(
        Tier::Default,
        "idle_compaction_yields_when_activity_resumes",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "idle_compaction_yields_when_activity_resumes",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    import_images(&workspace, &library, &mut report, 6, "维护");

    let scenario = headless(&workspace, "scenario")
        .action("idle_yield")
        .library(&library)
        .request(json!({ "timeoutMs": 10000 }))
        .env("LILITH_IDLE_THRESHOLD_MS", "800")
        .env("LILITH_IDLE_POLL_MS", "150")
        .timeout(std::time::Duration::from_secs(45))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    assert_eq!(data["yielded"]["phase"], "yielded", "{data}");
    assert!(data["garbageBefore"].as_f64().unwrap() > 0.2, "{data}");
    assert!(data["lastCompact"].is_object(), "{data}");
    assert!(
        data["garbageAfter"].as_f64().unwrap() < data["garbageBefore"].as_f64().unwrap(),
        "{data}"
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn opening_library_cleans_orphan_thumb_segments() {
    if skip_unless(Tier::Cheap, "opening_library_cleans_orphan_thumb_segments") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "opening_library_cleans_orphan_thumb_segments",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    import_images(&workspace, &library, &mut report, 2, "维护");
    let thumbs = library.join(".artbank").join("thumbs");
    let packs: Vec<std::path::PathBuf> = fs::read_dir(&thumbs)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .collect();
    assert!(!packs.is_empty(), "导入后应有缩略图包");
    let residue = thumbs.join("9999.pack");
    fs::write(&residue, [1_u8; 8]).unwrap();

    let pack = headless(&workspace, "maintenance")
        .action("pack")
        .library(&library)
        .finish();
    report.record(&pack);
    pack.expect_ok();
    assert!(!residue.exists(), "开库应清掉未登记的段文件");
    for path in packs {
        assert!(path.is_file(), "真实段不能被动：{}", path.display());
    }

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn heavy_check_9455_files() {
    if skip_unless(Tier::Heavy, "heavy_check_9455_files") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new("heavy_check_9455_files", Tier::Heavy, &workspace);
    let (library, _) = create_library(&workspace);
    let seeded = headless(&workspace, "fixture")
        .action("seed-db")
        .library(&library)
        .request(json!({
            "folders": ["素材"],
            "images": 9_455,
            "tagsPerImage": 0,
            "extensions": ["jpg"],
            "writeFiles": true,
        }))
        .timeout(std::time::Duration::from_secs(180))
        .finish();
    report.record(&seeded);
    seeded.expect_ok();
    assert_eq!(seeded.data()["files"], 9_455);

    let source = library.join("素材");
    let victim = fs::read_dir(&source)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .next()
        .expect("至少有一个占位文件");
    fs::remove_file(&victim).unwrap();
    let orphan_name = format!("{}.png", "d".repeat(64));
    fs::write(library.join(&orphan_name), [4_u8; 3]).unwrap();

    let checked = headless(&workspace, "maintenance")
        .action("check")
        .library(&library)
        .request(json!({ "mode": "full" }))
        .timeout(std::time::Duration::from_secs(180))
        .finish();
    report.record(&checked);
    checked.expect_ok();
    let found = &checked.data()["report"];
    assert_eq!(found["scannedRows"], 9_455);
    assert!(found["missingTotal"].as_u64().unwrap() >= 1, "{found}");
    assert!(found["orphanTotal"].as_u64().unwrap() >= 1, "{found}");
    assert!(
        found["hashMismatchTotal"].as_u64().unwrap() >= 9_000,
        "占位文件内容与哈希名不符，完整校验应全部报出：{found}"
    );

    workspace.assert_within_budget();
    report.success();
}

/// 连续 5 张失败就停下：终态必须是「部分失败」并给出未处理张数，不能说成已完成。
#[test]
fn thumbnail_rebuild_stops_after_repeated_failures() {
    if skip_unless(
        Tier::Cheap,
        "thumbnail_rebuild_stops_after_repeated_failures",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "thumbnail_rebuild_stops_after_repeated_failures",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed(&workspace, &library, &mut report, 8);
    for path in image_files(&workspace, &library) {
        fs::remove_file(&path).unwrap();
    }

    let rebuilt = headless(&workspace, "maintenance")
        .action("thumbnails")
        .library(&library)
        .request(json!({ "overwrite": true }))
        .finish();
    report.record(&rebuilt);
    rebuilt.expect_ok();
    let task = &rebuilt.data()["task"];
    assert_eq!(task["phase"], "partialFailed", "{task}");
    assert_eq!(task["failed"], 5, "连续 5 张失败后应当停下：{task}");
    assert!(
        task["message"]
            .as_str()
            .unwrap_or_default()
            .contains("未处理"),
        "要说明还剩多少张没做：{task}"
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn zero_tags_are_deleted_and_recorded() {
    if skip_unless(Tier::Cheap, "zero_tags_are_deleted_and_recorded") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "zero_tags_are_deleted_and_recorded",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let seeded = headless(&workspace, "fixture")
        .action("seed-db")
        .library(&library)
        .request(json!({
            "folders": ["人物"],
            "tags": ["甲", "乙", "丙"],
            "images": 2,
            "tagsPerImage": 1,
            "extensions": ["png"],
            "writeFiles": true,
        }))
        .finish();
    report.record(&seeded);
    seeded.expect_ok();

    let cleaned = headless(&workspace, "maintenance")
        .action("zero-tags")
        .library(&library)
        .finish();
    report.record(&cleaned);
    cleaned.expect_ok();
    assert_eq!(cleaned.data(), 1);

    let tags = headless(&workspace, "query")
        .action("tags")
        .library(&library)
        .request(json!({ "includeEmpty": true }))
        .finish();
    report.record(&tags);
    tags.expect_ok();
    let names: Vec<&str> = tags
        .data()
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["乙", "甲"]);
    assert!(
        history_entries(&workspace, &library)
            .iter()
            .any(|entry| entry["kind"] == "tagDelete"),
        "清理 0 张标签要写记录"
    );

    workspace.assert_within_budget();
    report.success();
}

fn seed(workspace: &Workspace, library: &Path, report: &mut Report, images: usize) {
    let outcome = headless(workspace, "fixture")
        .action("seed-db")
        .library(library)
        .request(json!({
            "folders": ["素材"],
            "images": images,
            "tagsPerImage": 0,
            "extensions": ["jpg"],
            "writeFiles": true,
        }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
}

fn image_files(workspace: &Workspace, library: &Path) -> Vec<std::path::PathBuf> {
    let page = headless(workspace, "query")
        .action("images")
        .library(library)
        .finish();
    page.expect_ok();
    page.data()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| std::path::PathBuf::from(row["filePath"].as_str().unwrap()))
        .collect()
}

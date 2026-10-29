#![cfg(feature = "headless")]

mod acceptance_support;

use std::collections::BTreeSet;

use serde_json::json;

use acceptance_support::{
    Report, Tier, Workspace, create_library, headless, history_entries, library_counts,
    skip_unless, write_corpus,
};

#[test]
fn import_small_corpus_with_tags_folder_and_duplicate_skip() {
    if skip_unless(
        Tier::Default,
        "import_small_corpus_with_tags_folder_and_duplicate_skip",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "import_small_corpus_with_tags_folder_and_duplicate_skip",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let corpus = workspace.scratch().join("in");
    write_corpus(&workspace, &mut report, &corpus, 6, &["png", "jpg"]);

    let imported = headless(&workspace, "import")
        .action("run")
        .library(&library)
        .request(json!({
            "paths": [corpus.to_string_lossy()],
            "folder": "导入",
            "tags": ["角色"],
            "score": 3,
        }))
        .finish();
    report.record(&imported);
    imported.expect_ok();
    let job = &imported.data()["job"];
    assert_eq!(job["phase"], "completed", "{job}");
    assert_eq!(job["imported"], 6);
    assert_eq!(job["failed"], 0);

    let counts = library_counts(&workspace, &library, &mut report);
    assert_eq!(counts["imageCount"], 6);

    let page = headless(&workspace, "query")
        .action("images")
        .library(&library)
        .finish();
    report.record(&page);
    page.expect_ok();
    let items = page.data()["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    for row in items {
        assert_eq!(row["folderPath"], "导入");
        assert_eq!(row["score"], 3);
        assert!(
            row["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tag| tag == "角色"),
            "{row}"
        );
        let file = library.join("导入").join(format!(
            "{}.{}",
            row["contentHash"].as_str().unwrap(),
            row["fileExt"].as_str().unwrap()
        ));
        assert!(file.is_file(), "{}", file.display());
    }

    let duplicate = headless(&workspace, "import")
        .action("run")
        .library(&library)
        .request(json!({
            "paths": [corpus.to_string_lossy()],
            "folder": "导入",
        }))
        .finish();
    report.record(&duplicate);
    duplicate.expect_ok();
    assert_eq!(duplicate.data()["job"]["imported"], 0);
    assert_eq!(duplicate.data()["job"]["duplicates"], 6);

    let history = history_entries(&workspace, &library);
    assert!(
        history.iter().any(|entry| entry["kind"] == "import"),
        "{history:?}"
    );
    assert!(
        history.iter().all(|entry| entry["kind"] != "trash"),
        "导入不应产生回收站记录"
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn import_preserve_keeps_relative_directories() {
    if skip_unless(Tier::Default, "import_preserve_keeps_relative_directories") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "import_preserve_keeps_relative_directories",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let input = workspace.scratch().join("tree");
    write_corpus(&workspace, &mut report, &input.join("a"), 3, &["png"]);
    write_corpus(&workspace, &mut report, &input.join("b"), 3, &["jpg"]);

    let imported = headless(&workspace, "import")
        .action("run")
        .library(&library)
        .request(json!({
            "paths": [input.to_string_lossy()],
            "folder": "素材",
            "folderMode": "preserve",
        }))
        .finish();
    report.record(&imported);
    imported.expect_ok();
    assert_eq!(imported.data()["job"]["imported"], 6);

    let page = headless(&workspace, "query")
        .action("images")
        .library(&library)
        .finish();
    report.record(&page);
    page.expect_ok();
    let folders: BTreeSet<&str> = page.data()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["folderPath"].as_str().unwrap())
        .collect();
    assert_eq!(
        folders,
        BTreeSet::from(["素材/a", "素材/b"]),
        "保留模式必须复现输入目录层级"
    );

    // 临时导入用与前面不同尺寸、不同格式的样本，避免与库内内容撞哈希。
    for (name, width, height, format) in [("a.png", 40, 30, "png"), ("b.jpg", 40, 30, "jpg")] {
        let outcome = headless(&workspace, "fixture")
            .action("write-image")
            .request(json!({
                "path": input.join("c").join(name).to_string_lossy(),
                "width": width,
                "height": height,
                "format": format,
            }))
            .finish();
        report.record(&outcome);
        outcome.expect_ok();
    }
    let temp = headless(&workspace, "import")
        .action("run")
        .library(&library)
        .request(json!({
            "paths": [input.join("c").to_string_lossy()],
            "target": "temp",
        }))
        .finish();
    report.record(&temp);
    temp.expect_ok();
    assert_eq!(temp.data()["job"]["imported"], 2);
    // 临时区在进程退出/关库时自动清空，只能在同一次导入的进程内观察计数。
    assert_eq!(temp.data()["counts"]["tempCount"], 2);
    assert_eq!(temp.data()["counts"]["imageCount"], 6);

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn import_cancel_keeps_committed_rows_and_clears_temp_on_reopen() {
    if skip_unless(
        Tier::Default,
        "import_cancel_keeps_committed_rows_and_clears_temp_on_reopen",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "import_cancel_keeps_committed_rows_and_clears_temp_on_reopen",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let corpus = workspace.scratch().join("many");
    write_corpus(&workspace, &mut report, &corpus, 120, &["png"]);

    let cancelled = headless(&workspace, "scenario")
        .action("import_cancel")
        .library(&library)
        .request(json!({
            "paths": [corpus.to_string_lossy()],
            "target": "library",
            "cancelAfterMs": 20,
        }))
        .timeout(std::time::Duration::from_secs(60))
        .finish();
    report.record(&cancelled);
    cancelled.expect_cancelled();
    let data = cancelled.data();
    assert_eq!(data["job"]["phase"], "cancelled", "{data}");
    assert_eq!(data["task"]["phase"], "cancelled", "{data}");
    let imported = data["job"]["imported"].as_u64().unwrap();
    assert!(imported <= 120);
    assert_eq!(data["countsBeforeClose"]["imageCount"], imported);
    let refused = data["refusedCloseError"].as_str().unwrap_or_default();
    assert!(
        refused.contains("请等待完成或取消"),
        "导入中关闭必须被拒绝：{refused}"
    );
    assert_eq!(data["reopened"]["imageCount"], imported);
    assert_eq!(data["reopened"]["tempCount"], 0);

    let temp = headless(&workspace, "scenario")
        .action("import_cancel")
        .library(&library)
        .request(json!({
            "paths": [corpus.to_string_lossy()],
            "target": "temp",
            "cancelAfterMs": 20,
        }))
        .timeout(std::time::Duration::from_secs(60))
        .finish();
    report.record(&temp);
    temp.expect_cancelled();
    let data = temp.data();
    let imported = data["job"]["imported"].as_u64().unwrap();
    assert_eq!(data["countsBeforeClose"]["tempCount"], imported);
    assert_eq!(data["reopened"]["tempCount"], 0, "{data}");
    assert_eq!(
        data["reopened"]["imageCount"], data["countsBeforeClose"]["imageCount"],
        "临时导入取消不该动图库行：{data}"
    );

    workspace.assert_within_budget();
    report.success();
}

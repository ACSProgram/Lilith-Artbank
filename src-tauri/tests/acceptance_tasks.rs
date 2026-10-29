#![cfg(feature = "headless")]

mod acceptance_support;

use serde_json::json;

use acceptance_support::{
    Report, Tier, Workspace, create_library, folder_id, headless, image_ids, seed_library,
    skip_unless, write_corpus,
};

#[test]
fn queued_and_running_imports_cancel_and_release_resources() {
    if skip_unless(
        Tier::Default,
        "queued_and_running_imports_cancel_and_release_resources",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "queued_and_running_imports_cancel_and_release_resources",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let corpus = workspace.scratch().join("many");
    write_corpus(&workspace, &mut report, &corpus, 150, &["png"]);

    let scenario = headless(&workspace, "scenario")
        .action("task_queue_cancel")
        .library(&library)
        .request(json!({ "paths": [corpus.to_string_lossy()] }))
        .timeout(std::time::Duration::from_secs(60))
        .finish();
    report.record(&scenario);
    scenario.expect_cancelled();
    let data = scenario.data();
    assert_eq!(data["secondPhaseWhileQueued"], "queued", "{data}");
    assert_eq!(data["first"]["phase"], "cancelled", "{data}");
    assert_eq!(data["second"]["phase"], "cancelled", "{data}");
    assert_eq!(data["idle"], true, "{data}");
    let imported = data["counts"]["imageCount"].as_u64().unwrap();
    assert!(imported <= 150, "{data}");

    let small = workspace.scratch().join("small");
    write_corpus(&workspace, &mut report, &small, 3, &["png"]);
    let follow_up = headless(&workspace, "import")
        .action("run")
        .library(&library)
        .request(json!({ "paths": [small.to_string_lossy()] }))
        .finish();
    report.record(&follow_up);
    follow_up.expect_ok();
    assert_eq!(follow_up.data()["job"]["imported"], 3);

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn task_panic_lands_failed_and_releases_guard() {
    if skip_unless(Tier::Cheap, "task_panic_lands_failed_and_releases_guard") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "task_panic_lands_failed_and_releases_guard",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed_library(
        &workspace,
        &library,
        json!({
            "folders": ["人物"],
            "images": 1,
            "tagsPerImage": 0,
            "writeFiles": true,
        }),
    );

    let scenario = headless(&workspace, "scenario")
        .action("task_panic")
        .library(&library)
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    assert_eq!(data["failed"]["phase"], "failed", "{data}");
    assert!(
        data["failed"]["message"]
            .as_str()
            .unwrap()
            .contains("任务异常终止"),
        "{data}"
    );
    assert_eq!(data["followUp"]["phase"], "completed", "{data}");
    assert_eq!(data["idle"], true, "{data}");

    let ids = image_ids(&workspace, &library);
    let score = headless(&workspace, "metadata")
        .action("score")
        .library(&library)
        .request(json!({ "imageId": ids[0], "score": 2 }))
        .finish();
    report.record(&score);
    score.expect_ok();

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn close_is_refused_while_write_task_runs_then_allowed() {
    if skip_unless(
        Tier::Cheap,
        "close_is_refused_while_write_task_runs_then_allowed",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "close_is_refused_while_write_task_runs_then_allowed",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);

    let scenario = headless(&workspace, "scenario")
        .action("close_during_task")
        .library(&library)
        .request(json!({ "holdMs": 300 }))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    let refused = data["refusedError"].as_str().unwrap_or_default();
    assert!(
        refused.contains("请等待完成或取消"),
        "写任务运行中关闭必须被拒绝：{data}"
    );
    assert_eq!(data["task"]["phase"], "completed", "{data}");
    assert_eq!(data["closedAfterFinish"], true, "{data}");
    assert_eq!(data["idle"], true, "{data}");

    workspace.assert_within_budget();
    report.success();
}

/// 排队任务在换库（代次前进）后拿到资源时必须直接取消，不能拿着旧句柄开工。
#[test]
fn queued_task_is_cancelled_after_the_library_epoch_moves() {
    if skip_unless(
        Tier::Cheap,
        "queued_task_is_cancelled_after_the_library_epoch_moves",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "queued_task_is_cancelled_after_the_library_epoch_moves",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);

    let scenario = headless(&workspace, "scenario")
        .action("epoch_switch")
        .library(&library)
        .request(json!({ "holdMs": 400 }))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    let queued = &data["queued"];
    assert_eq!(queued["phase"], "cancelled", "{data}");
    assert!(
        queued["message"]
            .as_str()
            .unwrap_or_default()
            .contains("图库已切换"),
        "排队任务要说明是因为换库才没做：{data}"
    );
    assert_eq!(data["closed"], true, "{data}");
    assert_eq!(data["libraryStillOpen"], false, "{data}");
    assert_eq!(data["idle"], true, "{data}");

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn folder_lock_blocks_rename_until_holder_finishes() {
    if skip_unless(
        Tier::Cheap,
        "folder_lock_blocks_rename_until_holder_finishes",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "folder_lock_blocks_rename_until_holder_finishes",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed_library(
        &workspace,
        &library,
        json!({
            "folders": ["角色"],
            "images": 2,
            "tagsPerImage": 0,
            "writeFiles": true,
        }),
    );
    let locked_folder = folder_id(&workspace, &library, &mut report, "角色");

    let scenario = headless(&workspace, "scenario")
        .action("folder_lock")
        .library(&library)
        .request(json!({
            "folderId": locked_folder,
            "folderPath": "角色",
            "newName": "角色改",
            // 文件夹改动最长等 10 秒；持锁更久才能验证超时拒绝。
            "holdMs": 11000,
        }))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    let blocked = data["blockedError"].as_str().unwrap_or_default();
    assert!(
        blocked.contains("导入") || blocked.contains("角色"),
        "持锁期间的改名必须被拒绝：{data}"
    );
    assert_eq!(data["renamed"], true, "{data}");
    assert_eq!(data["renamedPath"], "角色改", "{data}");
    assert_eq!(data["idle"], true, "{data}");
    assert_eq!(
        folder_id(&workspace, &library, &mut report, "角色改"),
        locked_folder
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn ai_holder_allows_metadata_write_and_blocks_content_write() {
    if skip_unless(
        Tier::Cheap,
        "ai_holder_allows_metadata_write_and_blocks_content_write",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "ai_holder_allows_metadata_write_and_blocks_content_write",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    seed_library(
        &workspace,
        &library,
        json!({
            "folders": ["人物"],
            "images": 1,
            "tagsPerImage": 0,
            "writeFiles": true,
        }),
    );
    let ids = image_ids(&workspace, &library);

    let scenario = headless(&workspace, "scenario")
        .action("ai_lock")
        .library(&library)
        .request(json!({ "imageId": ids[0], "holdMs": 300 }))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    assert_eq!(data["metadataWrite"], true, "{data}");
    let blocked = data["contentBlockedError"].as_str().unwrap_or_default();
    assert!(!blocked.is_empty(), "AI 期间内容写入必须被拒绝：{data}");
    assert_eq!(data["idle"], true, "{data}");

    let detail = headless(&workspace, "query")
        .action("detail")
        .library(&library)
        .request(json!({ "imageId": ids[0] }))
        .finish();
    report.record(&detail);
    detail.expect_ok();
    assert_eq!(detail.data()["score"], 3);

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn worker_threads_setting_applies_to_import() {
    if skip_unless(Tier::Default, "worker_threads_setting_applies_to_import") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "worker_threads_setting_applies_to_import",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);

    let settings = headless(&workspace, "settings")
        .action("get")
        .library(&library)
        .finish();
    report.record(&settings);
    settings.expect_ok();
    let mut value = settings.data().clone();
    value["performance"]["workerThreads"] = json!(2);
    let saved = headless(&workspace, "settings")
        .action("save")
        .library(&library)
        .request(value)
        .finish();
    report.record(&saved);
    saved.expect_ok();
    assert_eq!(saved.data()["performance"]["workerThreads"], 2);

    let corpus = workspace.scratch().join("workers");
    write_corpus(&workspace, &mut report, &corpus, 6, &["png"]);
    let imported = headless(&workspace, "import")
        .action("run")
        .library(&library)
        .request(json!({ "paths": [corpus.to_string_lossy()] }))
        .finish();
    report.record(&imported);
    imported.expect_ok();
    assert_eq!(imported.data()["job"]["imported"], 6);
    workspace.assert_within_budget();
    report.success();
}

#![cfg(feature = "headless")]

mod acceptance_support;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};

use acceptance_support::{Report, Tier, Workspace, create_library, headless, skip_unless};

#[test]
fn protocol_retries_validates_and_reports_offline() {
    if skip_unless(
        Tier::Cheap,
        "protocol_retries_validates_and_reports_offline",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "protocol_retries_validates_and_reports_offline",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    acceptance_support::ai_stub::reset_retry_attempts();
    let mut stub = acceptance_support::ai_stub::Stub::start();
    configure_ai(&workspace, &stub, &mut report);

    let status = headless(&workspace, "ai")
        .action("status")
        .library(&library)
        .finish();
    report.record(&status);
    status.expect_ok();
    assert_eq!(status.data()["online"], true, "{status:?}");
    assert_eq!(status.data()["health"]["status"], "ok", "{status:?}");
    assert_eq!(status.data()["launchConfigured"], false, "{status:?}");

    let health = headless(&workspace, "ai").action("health").finish();
    report.record(&health);
    health.expect_ok();
    assert_eq!(
        health.data()["capabilities"]["imageEmbedding"]["dim"],
        8,
        "{health:?}"
    );

    let ensured = headless(&workspace, "ai")
        .action("ensure")
        .request(json!({ "capabilities": ["imageEmbedding", "pose"] }))
        .finish();
    report.record(&ensured);
    ensured.expect_ok();
    assert_eq!(ensured.data()["imageEmbedding"]["ready"], true);
    assert_eq!(ensured.data()["pose"]["model"], "stub-pose");

    let embedded = headless(&workspace, "ai")
        .action("embed-text")
        .request(json!({ "model": "stub-image", "text": "hello" }))
        .finish();
    report.record(&embedded);
    embedded.expect_ok();
    assert_eq!(embedded.data()["dim"], 8);
    assert_eq!(embedded.data()["vector"].as_array().unwrap().len(), 8);

    let before = stub.requests_to("/v1/embed/text").len();
    let retried = headless(&workspace, "ai")
        .action("embed-text")
        .request(json!({ "model": "stub-image", "text": "retry please" }))
        .finish();
    report.record(&retried);
    retried.expect_ok();
    assert_eq!(
        stub.requests_to("/v1/embed/text").len() - before,
        3,
        "5xx 应再试两次"
    );

    let before = stub.requests_to("/v1/embed/text").len();
    let rejected = headless(&workspace, "ai")
        .action("embed-text")
        .request(json!({ "model": "stub-image", "text": "reject this" }))
        .finish();
    report.record(&rejected);
    rejected.expect_failed("请求被拒绝");
    assert_eq!(
        stub.requests_to("/v1/embed/text").len() - before,
        1,
        "4xx 不应重试"
    );

    let before = stub.requests_to("/v1/embed/text").len();
    let bad_dim = headless(&workspace, "ai")
        .action("embed-text")
        .request(json!({ "model": "stub-image", "text": "bad-dim" }))
        .finish();
    report.record(&bad_dim);
    bad_dim.expect_failed("维度不匹配");
    assert_eq!(stub.requests_to("/v1/embed/text").len() - before, 1);

    let shutdown = headless(&workspace, "ai").action("shutdown").finish();
    report.record(&shutdown);
    shutdown.expect_ok();
    assert_eq!(stub.requests_to("/v1/shutdown").len(), 1);

    stub.stop();
    let offline = headless(&workspace, "ai")
        .action("status")
        .library(&library)
        .finish();
    report.record(&offline);
    offline.expect_ok();
    assert_eq!(offline.data()["online"], false, "{offline:?}");
    let search = headless(&workspace, "ai")
        .action("search")
        .library(&library)
        .request(json!({ "text": "hello" }))
        .finish();
    report.record(&search);
    search.expect_failed("AI 服务未启动或无法连接");

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn batch_features_search_similar_groups_and_temp_area() {
    if skip_unless(
        Tier::Default,
        "batch_features_search_similar_groups_and_temp_area",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "batch_features_search_similar_groups_and_temp_area",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let mixed = workspace.scratch().join("mixed");
    write_corpus(&workspace, &mut report, &mixed, 4);
    let near_a = write_image(&workspace, &mut report, &mixed, "near-a", 0);
    let near_b = write_image(&workspace, &mut report, &mixed, "near-b", 1);
    let imported = headless(&workspace, "import")
        .action("run")
        .library(&library)
        .request(json!({
            "paths": [mixed.to_string_lossy()],
            "perceptualThreshold": 0,
        }))
        .finish();
    report.record(&imported);
    imported.expect_ok();
    assert_eq!(imported.data()["job"]["imported"], 6);
    let rows = image_rows(&workspace, &library, &mut report);
    let near_a_id = row_id(&rows, &near_a);
    let near_b_id = row_id(&rows, &near_b);

    let stub = acceptance_support::ai_stub::Stub::start();
    configure_ai(&workspace, &stub, &mut report);

    let embedded = headless(&workspace, "ai")
        .action("embed")
        .library(&library)
        .request(json!({ "scope": { "type": "library" } }))
        .timeout(Duration::from_secs(60))
        .finish();
    report.record(&embedded);
    embedded.expect_ok();
    assert_eq!(
        embedded.data()["task"]["phase"],
        "completed",
        "{embedded:?}"
    );
    assert_eq!(embedded.data()["task"]["completed"], 6);
    assert_eq!(embedded.data()["features"]["library"]["embedding"], 6);

    let posed = headless(&workspace, "ai")
        .action("pose")
        .library(&library)
        .request(json!({
            "scope": { "type": "selected", "imageIds": [near_a_id, near_b_id] },
        }))
        .timeout(Duration::from_secs(60))
        .finish();
    report.record(&posed);
    posed.expect_ok();
    assert_eq!(posed.data()["task"]["completed"], 2);
    assert_eq!(posed.data()["features"]["library"]["pose"], 2);
    let poses = headless(&workspace, "ai")
        .action("pose-get")
        .library(&library)
        .request(json!({ "imageIds": [near_a_id, near_b_id] }))
        .finish();
    report.record(&poses);
    poses.expect_ok();
    assert_eq!(
        poses.data()["poses"][0]["pose"]["landmarks"]
            .as_array()
            .unwrap()
            .len(),
        33,
        "{poses:?}"
    );

    let described = headless(&workspace, "ai")
        .action("describe")
        .library(&library)
        .request(json!({ "imageId": near_a_id }))
        .finish();
    report.record(&described);
    described.expect_ok();
    assert_eq!(described.data()["text"], "stub 描述");
    let detail = headless(&workspace, "query")
        .action("detail")
        .library(&library)
        .request(json!({ "imageId": near_a_id }))
        .finish();
    report.record(&detail);
    detail.expect_ok();
    assert_eq!(detail.data()["description"], "stub 描述", "{detail:?}");

    let flow = headless(&workspace, "scenario")
        .action("ai_search_flow")
        .library(&library)
        .request(json!({
            "text": "a red cloak",
            "limit": 6,
            "pageOffset": 3,
            "pageLimit": 10,
            "similar": {
                "kind": "embedding",
                "targetIds": [near_a_id],
                "limit": 10,
            },
        }))
        .finish();
    report.record(&flow);
    flow.expect_ok();
    let data = flow.data();
    assert_eq!(data["search"]["total"], 6, "{data}");
    assert_eq!(data["search"]["totalHits"], 6, "{data}");
    assert_eq!(data["search"]["meanCount"], 6, "{data}");
    assert_eq!(data["page"]["items"].as_array().unwrap().len(), 3);
    assert!(data["page"]["nextOffset"].is_null(), "{data}");
    assert_eq!(data["ids"].as_array().unwrap().len(), 6);
    assert_eq!(data["similar"]["totalHits"], 5, "{data}");
    assert_eq!(data["similar"]["total"], 5, "{data}");
    assert_eq!(
        data["similarPage"]["items"].as_array().unwrap().len(),
        2,
        "{data}"
    );

    let multi = headless(&workspace, "ai")
        .action("similar")
        .library(&library)
        .request(json!({
            "kind": "phash",
            "targetIds": [near_a_id, near_b_id],
        }))
        .finish();
    report.record(&multi);
    multi.expect_failed("只支持单张参考图");

    let groups = headless(&workspace, "ai")
        .action("groups")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "groupPhash": true,
            "phashThreshold": 7,
            "groupEmbedding": true,
            "embeddingThreshold": 0.5,
        }))
        .timeout(Duration::from_secs(60))
        .finish();
    report.record(&groups);
    groups.expect_ok();
    let groups_value = groups.data()["groups"].as_array().unwrap();
    assert!(
        groups_value.iter().any(|group| {
            let ids = group.as_array().unwrap();
            ids.contains(&json!(near_a_id)) && ids.contains(&json!(near_b_id))
        }),
        "近似重复图应归到一组：{groups:?}"
    );

    let scoped = headless(&workspace, "ai")
        .action("groups")
        .library(&library)
        .request(json!({
            "scope": { "type": "selected", "imageIds": [near_a_id] },
            "groupPhash": true,
        }))
        .timeout(Duration::from_secs(30))
        .finish();
    report.record(&scoped);
    scoped.expect_ok();
    assert!(
        scoped.data()["groups"].as_array().unwrap().is_empty(),
        "范围只有一张图时不应有组：{scoped:?}"
    );

    let too_wide = headless(&workspace, "ai")
        .action("groups")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "groupPhash": true,
            "phashThreshold": 8,
        }))
        .finish();
    report.record(&too_wide);
    too_wide.expect_failed("只支持 0-7");

    let keeps = headless(&workspace, "ai")
        .action("keeps")
        .library(&library)
        .request(json!({ "groups": [[near_a_id, near_b_id]] }))
        .finish();
    report.record(&keeps);
    keeps.expect_ok();
    let kept = keeps.data().as_array().unwrap();
    assert_eq!(kept.len(), 1);
    assert!(kept[0].is_i64());

    stub.update_config(|config| config.text_model = "other-text".into());
    let mismatched = headless(&workspace, "ai")
        .action("search")
        .library(&library)
        .request(json!({ "text": "a red cloak" }))
        .finish();
    report.record(&mismatched);
    mismatched.expect_failed("不一致");
    stub.update_config(|config| config.text_model = config.image_model.clone());

    let temp_corpus = workspace.scratch().join("temp");
    write_corpus(&workspace, &mut report, &temp_corpus, 3);
    let temp_flow = headless(&workspace, "scenario")
        .action("ai_temp_flow")
        .library(&library)
        .request(json!({
            "paths": [temp_corpus.to_string_lossy()],
            "kind": "embedding",
        }))
        .timeout(Duration::from_secs(60))
        .finish();
    report.record(&temp_flow);
    temp_flow.expect_ok();
    let data = temp_flow.data();
    assert_eq!(data["countsBefore"]["tempCount"], 3, "{data}");
    assert_eq!(data["tempIds"].as_array().unwrap().len(), 3, "{data}");
    assert_eq!(data["featuresTask"]["completed"], 3, "{data}");
    assert_eq!(data["tempEmbeddings"], 3, "{data}");
    assert_eq!(data["tempSearch"]["totalHits"], 3, "{data}");
    assert_eq!(data["librarySearch"]["totalHits"], 6, "{data}");
    assert_eq!(
        data["countsAfterReopen"]["tempCount"], 0,
        "关闭图库应清空临时区：{data}"
    );

    workspace.assert_within_budget();
    report.success();
}

fn configure_ai(
    workspace: &Workspace,
    stub: &acceptance_support::ai_stub::Stub,
    report: &mut Report,
) {
    let settings = headless(workspace, "settings").action("get").finish();
    report.record(&settings);
    settings.expect_ok();
    let mut value = settings.data().clone();
    value["ai"]["baseUrl"] = json!(stub.url());
    value["ai"]["port"] = json!(stub.port());
    value["ai"]["autoStart"] = json!(false);
    value["ai"]["pythonPath"] = Value::Null;
    value["ai"]["serverScript"] = Value::Null;
    value["ai"]["modelPath"] = Value::Null;
    value["ai"]["clipModelPath"] = Value::Null;
    value["ai"]["prewarmImports"] = json!(false);
    let saved = headless(workspace, "settings")
        .action("save")
        .request(value)
        .finish();
    report.record(&saved);
    saved.expect_ok();
}

fn write_corpus(workspace: &Workspace, report: &mut Report, directory: &Path, count: usize) {
    let outcome = headless(workspace, "fixture")
        .action("corpus")
        .request(json!({
            "directory": directory.to_string_lossy(),
            "count": count,
            "width": 32,
            "height": 24,
            "formats": ["png"],
        }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
}

fn write_image(
    workspace: &Workspace,
    report: &mut Report,
    directory: &Path,
    stem: &str,
    tweak: u32,
) -> PathBuf {
    let path = directory.join(format!("{stem}.png"));
    let outcome = headless(workspace, "fixture")
        .action("write-image")
        .request(json!({
            "path": path.to_string_lossy(),
            "width": 32,
            "height": 24,
            "format": "png",
            "seed": 0,
            "tweak": tweak,
        }))
        .finish();
    report.record(&outcome);
    outcome.expect_ok();
    path
}

fn image_rows(workspace: &Workspace, library: &Path, report: &mut Report) -> Vec<Value> {
    let page = headless(workspace, "query")
        .action("images")
        .library(library)
        .request(json!({ "limit": 100 }))
        .finish();
    report.record(&page);
    page.expect_ok();
    page.data()["items"].as_array().cloned().unwrap_or_default()
}

fn row_id(rows: &[Value], source: &Path) -> i64 {
    let bytes =
        fs::read(source).unwrap_or_else(|error| panic!("读取 {} 失败：{error}", source.display()));
    let hash = blake3::hash(&bytes).to_hex().to_string();
    rows.iter()
        .find(|row| row["contentHash"] == hash.as_str())
        .unwrap_or_else(|| panic!("找不到 {} 对应的行：{rows:?}", source.display()))["id"]
        .as_i64()
        .unwrap()
}

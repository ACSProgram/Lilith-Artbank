#![cfg(feature = "headless")]

mod acceptance_support;

use std::{collections::HashMap, path::Path, time::Duration};

use serde_json::{Value, json};

use acceptance_support::{
    Report, Tier, Workspace, create_library, headless, image_ids, skip_unless,
};

#[test]
fn estimate_and_execute_replace_sources_and_record_history() {
    if skip_unless(
        Tier::Default,
        "estimate_and_execute_replace_sources_and_record_history",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "estimate_and_execute_replace_sources_and_record_history",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let ids = import_textures(&workspace, &library, &mut report, 12, 256);
    let before = row_facts(&workspace, &library, &mut report);

    let scenario = headless(&workspace, "scenario")
        .action("optimize_flow")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "sampleSize": 50,
            "options": { "tier": "high", "outputFormat": "webp" },
        }))
        .timeout(Duration::from_secs(180))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    let estimate = &data["estimate"];
    assert_eq!(estimate["candidateCount"], 12, "{data}");
    assert_eq!(estimate["processCount"], 12, "{data}");
    assert!(estimate["savingsBytes"].as_u64().unwrap() > 0, "{data}");
    assert!(estimate["sampleCount"].as_u64().unwrap() > 0, "{data}");
    let options_fingerprint = estimate["optionsFingerprint"].as_str().unwrap();
    let candidate_fingerprint = estimate["candidateFingerprint"].as_str().unwrap();
    assert_eq!(options_fingerprint.len(), 64, "{data}");
    assert_eq!(candidate_fingerprint.len(), 64, "{data}");
    assert!(data["error"].is_null(), "{data}");
    assert_eq!(data["run"]["phase"], "completed", "{data}");
    assert_eq!(data["run"]["completed"], 12, "{data}");
    assert!(data["run"]["savedBytes"].as_u64().unwrap() > 0, "{data}");
    assert!(
        data["run"]["message"]
            .as_str()
            .unwrap()
            .contains("已替换 12 张"),
        "{data}"
    );
    assert!(
        data["history"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "optimize"),
        "压缩要写操作记录：{data}"
    );

    let after = row_facts(&workspace, &library, &mut report);
    for id in &ids {
        let old = &before[id];
        let new = &after[id];
        let old_path = Path::new(old["filePath"].as_str().unwrap());
        let new_path = Path::new(new["filePath"].as_str().unwrap());
        assert_ne!(old_path, new_path, "内容哈希变了，文件名也应变化：{id}");
        assert_eq!(new["format"], "webp", "{new}");
        assert_eq!(
            new_path.extension().and_then(|ext| ext.to_str()),
            Some("webp")
        );
        assert!(!old_path.exists(), "旧原图应被删除：{}", old_path.display());
        assert!(new_path.is_file(), "新原图应存在：{}", new_path.display());
        assert!(
            new["fileSize"].as_u64().unwrap() < old["fileSize"].as_u64().unwrap(),
            "压缩后应更小：{old} -> {new}"
        );
    }

    // 第二次估算应把已处理的图片全部算进「已优化」。
    let again = headless(&workspace, "optimize")
        .action("estimate")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "sampleSize": 50,
            "options": { "tier": "high", "outputFormat": "webp" },
        }))
        .timeout(Duration::from_secs(120))
        .finish();
    report.record(&again);
    again.expect_ok();
    let estimate = &again.data()["estimate"];
    assert_eq!(estimate["candidateCount"], 0, "{estimate}");
    assert_eq!(estimate["skipOptimized"]["count"], 12, "{estimate}");

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn stale_grant_and_source_drift_are_rejected_or_trimmed() {
    if skip_unless(
        Tier::Default,
        "stale_grant_and_source_drift_are_rejected_or_trimmed",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "stale_grant_and_source_drift_are_rejected_or_trimmed",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let ids = import_textures(&workspace, &library, &mut report, 120, 64);

    let options = json!({ "tier": "high", "outputFormat": "webp" });
    let options_changed = headless(&workspace, "scenario")
        .action("optimize_flow")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "sampleSize": 50,
            "options": options,
            "runOptions": { "tier": "medium", "outputFormat": "webp" },
        }))
        .timeout(Duration::from_secs(180))
        .finish();
    report.record(&options_changed);
    options_changed.expect_ok();
    let error = options_changed.data()["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("范围或参数已改变"),
        "换参数后执行必须被拒绝：{}",
        options_changed.describe()
    );

    let candidate_changed = headless(&workspace, "scenario")
        .action("optimize_flow")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "sampleSize": 50,
            "options": options,
            "mutate": { "trashImageIds": [ids[0]] },
        }))
        .timeout(Duration::from_secs(180))
        .finish();
    report.record(&candidate_changed);
    candidate_changed.expect_ok();
    let error = candidate_changed.data()["error"]
        .as_str()
        .unwrap_or_default();
    assert!(
        error.contains("另一批") || error.contains("候选图片已变化"),
        "换了一批图后执行必须被拒绝：{}",
        candidate_changed.describe()
    );

    let partial = headless(&workspace, "scenario")
        .action("optimize_flow")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "sampleSize": 50,
            "options": options,
            "mutate": { "truncateImageIds": [ids[1]] },
        }))
        .timeout(Duration::from_secs(180))
        .finish();
    report.record(&partial);
    partial.expect_ok();
    let data = partial.data();
    assert!(data["error"].is_null(), "{data}");
    assert_eq!(data["run"]["phase"], "completed", "{data}");
    assert!(
        data["run"]["message"]
            .as_str()
            .unwrap()
            .contains("未纳入候选 1 张"),
        "少量原图漂移应剔除后继续：{data}"
    );

    let rejected = headless(&workspace, "scenario")
        .action("optimize_flow")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "sampleSize": 50,
            "options": options,
            "mutate": { "truncateImageIds": [ids[2], ids[3], ids[4]] },
        }))
        .timeout(Duration::from_secs(180))
        .finish();
    report.record(&rejected);
    rejected.expect_ok();
    let error = rejected.data()["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("原图缺失或已变化") && error.contains("重新估算"),
        "超过 1% 的漂移必须拒绝：{}",
        rejected.describe()
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn no_candidates_and_temp_scope_fail_clearly() {
    if skip_unless(Tier::Cheap, "no_candidates_and_temp_scope_fail_clearly") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "no_candidates_and_temp_scope_fail_clearly",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let corpus = workspace.scratch().join("jpeg");
    let seeded = headless(&workspace, "fixture")
        .action("corpus")
        .request(json!({
            "directory": corpus.to_string_lossy(),
            "count": 6,
            "width": 256,
            "height": 256,
            "formats": ["jpg"],
        }))
        .finish();
    report.record(&seeded);
    seeded.expect_ok();
    let imported = headless(&workspace, "import")
        .action("run")
        .library(&library)
        .request(json!({ "paths": [corpus.to_string_lossy()] }))
        .finish();
    report.record(&imported);
    imported.expect_ok();

    let scenario = headless(&workspace, "scenario")
        .action("optimize_flow")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "sampleSize": 50,
        }))
        .timeout(Duration::from_secs(120))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    assert_eq!(data["estimate"]["candidateCount"], 6, "{data}");
    assert_eq!(data["estimate"]["processCount"], 0, "{data}");
    assert_eq!(data["estimate"]["skipNotSmaller"]["count"], 6, "{data}");
    assert!(data["error"].is_null(), "{data}");
    assert_eq!(data["run"]["phase"], "completed", "{data}");
    assert_eq!(data["run"]["skipped"], 6, "{data}");
    assert_eq!(data["run"]["savedBytes"], 0, "{data}");
    assert!(
        data["run"]["message"]
            .as_str()
            .unwrap()
            .contains("已跳过 6 张"),
        "全跳过时执行要说清原因：{data}"
    );

    let temp = headless(&workspace, "optimize")
        .action("estimate")
        .library(&library)
        .request(json!({ "scope": { "type": "temp" }, "sampleSize": 50 }))
        .finish();
    report.record(&temp);
    temp.expect_failed("临时文件夹不参与图片空间优化");

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn heavy_optimize_300_textures() {
    if skip_unless(Tier::Heavy, "heavy_optimize_300_textures") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new("heavy_optimize_300_textures", Tier::Heavy, &workspace);
    let (library, _) = create_library(&workspace);
    import_textures(&workspace, &library, &mut report, 300, 256);
    let scenario = headless(&workspace, "scenario")
        .action("optimize_flow")
        .library(&library)
        .request(json!({
            "scope": { "type": "library" },
            "sampleSize": 200,
            "options": { "tier": "high", "outputFormat": "webp" },
        }))
        .timeout(Duration::from_secs(600))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    assert!(data["error"].is_null(), "{data}");
    assert_eq!(data["run"]["phase"], "completed", "{data}");
    assert_eq!(data["run"]["completed"], 300, "{data}");
    workspace.assert_within_budget();
    report.success();
}

fn import_textures(
    workspace: &Workspace,
    library: &Path,
    report: &mut Report,
    count: usize,
    size: u32,
) -> Vec<i64> {
    let corpus = workspace.scratch().join(format!("tex-{size}"));
    let seeded = headless(workspace, "fixture")
        .action("corpus")
        .request(json!({
            "directory": corpus.to_string_lossy(),
            "count": count,
            "width": size,
            "height": size,
            "formats": ["bmp"],
        }))
        .timeout(Duration::from_secs(120))
        .finish();
    report.record(&seeded);
    seeded.expect_ok();
    let imported = headless(workspace, "import")
        .action("run")
        .library(library)
        .request(json!({ "paths": [corpus.to_string_lossy()] }))
        .timeout(Duration::from_secs(180))
        .finish();
    report.record(&imported);
    imported.expect_ok();
    assert_eq!(imported.data()["job"]["imported"], count, "{imported:?}");
    image_ids(workspace, library)
}

fn row_facts(workspace: &Workspace, library: &Path, report: &mut Report) -> HashMap<i64, Value> {
    let page = headless(workspace, "query")
        .action("images")
        .library(library)
        .request(json!({ "limit": 500 }))
        .finish();
    report.record(&page);
    page.expect_ok();
    page.data()["items"]
        .as_array()
        .expect("行列表")
        .iter()
        .map(|row| (row["id"].as_i64().expect("图片 id"), row.clone()))
        .collect()
}

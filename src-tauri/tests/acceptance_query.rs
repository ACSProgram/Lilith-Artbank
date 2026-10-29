#![cfg(feature = "headless")]

mod acceptance_support;

use serde_json::{Value, json};

use acceptance_support::{Outcome, Report, Tier, Workspace, headless, skip_unless};

const IMAGES: u64 = 120;

fn prepare(workspace: &Workspace, report: &mut Report) {
    let library = workspace.library();
    let created = headless(workspace, "library")
        .action("create")
        .request(json!({ "path": library.to_string_lossy(), "name": "查询库" }))
        .finish();
    report.record(&created);
    created.expect_ok();

    let seeded = headless(workspace, "fixture")
        .action("seed-db")
        .library(&library)
        .request(json!({
            "folders": ["人物", "人物/速写", "场景"],
            "tags": ["红", "蓝", "绿", "黄"],
            "images": IMAGES,
            "tagsPerImage": 2,
            "extensions": ["jpg", "png"],
            "writeFiles": true,
        }))
        .finish();
    report.record(&seeded);
    seeded.expect_ok();
    assert_eq!(seeded.data()["images"], IMAGES);
}

fn query(workspace: &Workspace, action: &str, request: impl serde::Serialize) -> Outcome {
    headless(workspace, "query")
        .action(action)
        .library(&workspace.library())
        .request(request)
        .finish()
}

#[test]
fn pages_search_ids_and_filters() {
    if skip_unless(Tier::Cheap, "pages_search_ids_and_filters") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new("pages_search_ids_and_filters", Tier::Cheap, &workspace);
    prepare(&workspace, &mut report);

    let first = query(&workspace, "images", json!({ "limit": 10 }));
    report.record(&first);
    first.expect_ok();
    let items = first.data()["items"].as_array().unwrap();
    assert_eq!(items.len(), 10);
    assert!(first.data()["nextCursor"].is_object());
    assert_eq!(items[0]["importedAt"], IMAGES as i64 - 1);

    let cursor = first.data()["nextCursor"].clone();
    let second = query(
        &workspace,
        "images",
        json!({ "limit": 10, "cursor": cursor }),
    );
    report.record(&second);
    second.expect_ok();
    let first_ids = items
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect::<Vec<_>>();
    let second_ids = second.data()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(second_ids.len(), 10);
    assert!(
        second_ids.iter().all(|id| !first_ids.contains(id)),
        "两页不应重复"
    );

    let png = query(
        &workspace,
        "ids",
        json!({ "filters": { "extensions": ["png"] } }),
    );
    report.record(&png);
    png.expect_ok();
    assert_eq!(png.data().as_array().unwrap().len(), 60);

    let zero_score = query(&workspace, "ids", json!({ "filters": { "scores": [0] } }));
    report.record(&zero_score);
    zero_score.expect_ok();
    assert_eq!(zero_score.data().as_array().unwrap().len(), 20);

    let search = query(
        &workspace,
        "stats",
        json!({
            "profile": "summary",
            "query": { "search": "第0张" },
        }),
    );
    report.record(&search);
    search.expect_ok();
    assert_eq!(search.data()["data"]["total"], 1);

    let no_tags = query(
        &workspace,
        "stats",
        json!({ "profile": "summary", "query": { "filters": { "tagPresence": "missing" } } }),
    );
    report.record(&no_tags);
    no_tags.expect_ok();
    assert_eq!(no_tags.data()["data"]["total"], 0);

    let tags = query(&workspace, "tags", json!({}));
    report.record(&tags);
    tags.expect_ok();
    let tags = tags.data().as_array().unwrap();
    assert_eq!(tags.len(), 4);
    assert!(tags.iter().all(|tag| tag["imageCount"] == 60));
    let red_id = tags.iter().find(|tag| tag["name"] == "红").expect("红标签")["id"]
        .as_i64()
        .unwrap();

    let without_red = query(
        &workspace,
        "ids",
        json!({ "filters": { "excludeTagIds": [red_id] } }),
    );
    report.record(&without_red);
    without_red.expect_ok();
    assert_eq!(without_red.data().as_array().unwrap().len(), 60);

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn stats_folders_detail_and_states() {
    if skip_unless(Tier::Cheap, "stats_folders_detail_and_states") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new("stats_folders_detail_and_states", Tier::Cheap, &workspace);
    prepare(&workspace, &mut report);

    let folders = query(&workspace, "folders", json!({}));
    report.record(&folders);
    folders.expect_ok();
    let folder_list = folders.data().as_array().unwrap();
    let person_id = folder_id(folder_list, "人物");
    let sketch_id = folder_id(folder_list, "人物/速写");
    assert!(sketch_id > person_id);

    let person = folder_list
        .iter()
        .find(|folder| folder["id"] == person_id)
        .unwrap();
    assert_eq!(person["directImageCount"], 40);
    assert_eq!(person["totalImageCount"], 80);
    let sketch = folder_list
        .iter()
        .find(|folder| folder["id"] == sketch_id)
        .unwrap();
    assert_eq!(sketch["directImageCount"], 40);
    assert_eq!(sketch["totalImageCount"], 40);

    let direct = query(
        &workspace,
        "ids",
        json!({
            "filters": { "folderIds": [person_id], "folderDirect": true },
        }),
    );
    report.record(&direct);
    direct.expect_ok();
    assert_eq!(direct.data().as_array().unwrap().len(), 40);

    let subtree = query(
        &workspace,
        "ids",
        json!({
            "filters": { "folderIds": [person_id] },
        }),
    );
    report.record(&subtree);
    subtree.expect_ok();
    assert_eq!(subtree.data().as_array().unwrap().len(), 80);

    let facets = query(
        &workspace,
        "stats",
        json!({ "profile": "facets", "query": {} }),
    );
    report.record(&facets);
    facets.expect_ok();
    let facets = &facets.data()["data"];
    assert_eq!(facets["total"], IMAGES);
    let png = facets["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["value"] == "png")
        .expect("png 分面");
    assert_eq!(png["count"], 60);
    assert_eq!(facets["scores"].as_array().unwrap().len(), 6);
    assert!(
        facets["scores"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["count"] == 20)
    );

    let detail_stats = query(
        &workspace,
        "stats",
        json!({
            "profile": "folderDetail",
            "query": { "filters": { "folderIds": [person_id] } },
        }),
    );
    report.record(&detail_stats);
    detail_stats.expect_ok();
    let detail_stats = &detail_stats.data()["data"];
    assert_eq!(detail_stats["imageCount"], 80);
    assert_eq!(detail_stats["totalBytes"], 80);
    assert_eq!(detail_stats["subfolderCount"], 1);
    assert_eq!(detail_stats["extensions"].as_array().unwrap().len(), 2);
    // 人物子树只含 index % 3 ∈ {0,1} 的图片，对应评分档只有 0/1/3/4。
    assert_eq!(detail_stats["scores"].as_array().unwrap().len(), 4);
    assert!(!detail_stats["tags"].as_array().unwrap().is_empty());

    let page = query(&workspace, "images", json!({ "limit": 3 }));
    report.record(&page);
    page.expect_ok();
    let row = &page.data()["items"][0];
    let image_id = row["id"].as_i64().unwrap();

    let detail = query(&workspace, "detail", json!({ "imageId": image_id }));
    report.record(&detail);
    detail.expect_ok();
    assert_eq!(detail.data()["id"], image_id);
    assert_eq!(detail.data()["importedAt"], row["importedAt"]);
    assert_eq!(detail.data()["score"], row["score"]);
    assert_eq!(detail.data()["folderPath"], row["folderPath"]);
    assert_eq!(detail.data()["contentHash"].as_str().unwrap().len(), 64);

    let states = query(
        &workspace,
        "states",
        json!({ "imageIds": [image_id, row["id"].as_i64().unwrap() - 1] }),
    );
    report.record(&states);
    states.expect_ok();
    assert_eq!(states.data().as_array().unwrap().len(), 2);
    assert!(
        states
            .data()
            .as_array()
            .unwrap()
            .iter()
            .all(|state| state["deletedAt"].is_null())
    );

    let selection = query(
        &workspace,
        "selection-tags",
        json!({ "imageIds": [image_id] }),
    );
    report.record(&selection);
    selection.expect_ok();
    assert_eq!(selection.data()["total"], 1);
    assert_eq!(selection.data()["tags"].as_array().unwrap().len(), 2);

    workspace.assert_within_budget();
    report.success();
}

fn folder_id(folders: &[Value], path: &str) -> i64 {
    folders
        .iter()
        .find(|folder| folder["path"] == path)
        .unwrap_or_else(|| panic!("缺少文件夹 {path}"))["id"]
        .as_i64()
        .unwrap()
}

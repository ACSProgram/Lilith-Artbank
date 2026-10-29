#![cfg(feature = "headless")]

mod acceptance_support;

use std::{net::TcpListener, time::Duration};

use serde_json::json;

use acceptance_support::{
    Report, Tier, Workspace, create_library, headless, history_entries, import_images, skip_unless,
};

#[test]
fn stdio_bridge_lists_and_calls_tools() {
    if skip_unless(Tier::Cheap, "stdio_bridge_lists_and_calls_tools") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "stdio_bridge_lists_and_calls_tools",
        Tier::Cheap,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    import_images(&workspace, &library, &mut report, 2, "桥");

    let scenario = headless(&workspace, "scenario")
        .action("mcp_bridge")
        .library(&library)
        .request(json!({ "port": free_port() }))
        .timeout(Duration::from_secs(60))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();
    assert_eq!(
        data["initialize"]["result"]["serverInfo"]["name"], "lilith-artbank",
        "{data}"
    );
    assert_eq!(data["toolCount"], 24, "{data}");
    assert_eq!(data["libraryInfo"]["mcp"]["mode"], "full", "{data}");
    assert_eq!(data["libraryInfo"]["mcp"]["writeAllowed"], true, "{data}");
    assert!(
        data["libraryInfo"]["mcp"]["approval"]["threshold"].is_null(),
        "{data}"
    );
    assert_eq!(data["search"]["total"], 2, "{data}");
    assert_eq!(data["tags"]["total"], 0, "{data}");
    assert_eq!(data["runtimeRemovedAfterStop"], true, "{data}");

    let audit = std::fs::read_to_string(workspace.app_root().join("logs").join("mcp.log"))
        .unwrap_or_default();
    assert!(audit.contains("[bridge]"), "小桥要写诊断：{audit}");
    assert!(
        audit.contains("tool=library_info mode=full status=ok"),
        "工具调用要进审计：{audit}"
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn modes_approvals_expiry_library_switch_and_restart() {
    if skip_unless(
        Tier::Default,
        "modes_approvals_expiry_library_switch_and_restart",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "modes_approvals_expiry_library_switch_and_restart",
        Tier::Default,
        &workspace,
    );
    let (library, _) = create_library(&workspace);
    let ids = import_images(&workspace, &library, &mut report, 2, "MCP");

    let scenario = headless(&workspace, "scenario")
        .action("mcp_modes")
        .library(&library)
        .request(json!({ "port": free_port(), "imageIds": ids }))
        .env("LILITH_MCP_APPROVAL_TTL_MS", "800")
        .timeout(Duration::from_secs(90))
        .finish();
    report.record(&scenario);
    scenario.expect_ok();
    let data = scenario.data();

    let read_only = &data["readOnly"];
    assert_eq!(read_only["libraryInfo"]["mcp"]["writeAllowed"], false);
    assert!(
        read_only["update"]
            .as_str()
            .unwrap_or_default()
            .contains("当前是只读模式"),
        "只读档写入应被拒绝：{read_only}"
    );

    let ask = &data["ask"];
    assert_eq!(ask["dryRun"]["selected"], ids.len() as u64, "{ask}");
    assert_eq!(ask["rejectedStatus"]["status"], "rejected", "{ask}");
    assert_eq!(ask["approved"]["status"], "ok", "{ask}");
    assert_eq!(ask["approvalStatus"]["status"], "completed", "{ask}");
    assert_eq!(
        ask["countsAfterApprove"]["trashCount"],
        ids.len() as u64,
        "{ask}"
    );
    assert_eq!(ask["expiredStatus"]["status"], "expired", "{ask}");
    assert!(
        ask["expiredError"]
            .as_str()
            .unwrap_or_default()
            .contains("已过期"),
        "{ask}"
    );
    assert_eq!(ask["approvalsAfterSwitch"], 0, "{ask}");

    let full = &data["full"];
    assert_eq!(full["libraryInfo"]["mcp"]["mode"], "full", "{full}");
    assert!(
        full["libraryInfo"]["mcp"]["approval"]["threshold"].is_null(),
        "{full}"
    );
    assert_eq!(full["trash"]["status"], "ok", "{full}");
    assert_eq!(
        full["countsAfterTrash"]["trashCount"],
        ids.len() as u64,
        "{full}"
    );

    let lifecycle = &data["lifecycle"];
    assert_eq!(lifecycle["tokenChanged"], true, "{lifecycle}");
    assert_eq!(lifecycle["runtimeRemovedAfterStop"], true, "{lifecycle}");
    assert!(
        lifecycle["callAfterStop"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("找不到 MCP 运行时文件"),
        "{lifecycle}"
    );

    let labels: Vec<String> = history_entries(&workspace, &library)
        .into_iter()
        .filter_map(|entry| entry["label"].as_str().map(str::to_owned))
        .collect();
    assert!(
        labels.iter().any(|label| label.starts_with("AI · ")),
        "MCP 写入要留带 AI 前缀的记录：{labels:?}"
    );

    let audit = std::fs::read_to_string(workspace.app_root().join("logs").join("mcp.log"))
        .unwrap_or_default();
    for expected in [
        "tool=library_info mode=readOnly status=ok",
        "tool=images_update mode=readOnly status=error",
        "tool=images_trash mode=ask status=pendingApproval",
        "[approval]",
    ] {
        assert!(audit.contains(expected), "审计缺少 {expected:?}：{audit}");
    }

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn occupied_port_keeps_settings_and_reports_error() {
    if skip_unless(
        Tier::Cheap,
        "occupied_port_keeps_settings_and_reports_error",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "occupied_port_keeps_settings_and_reports_error",
        Tier::Cheap,
        &workspace,
    );
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("无法占用测试端口");
    let port = listener.local_addr().unwrap().port();

    let settings = headless(&workspace, "settings").action("get").finish();
    report.record(&settings);
    settings.expect_ok();
    let mut value = settings.data().clone();
    value["mcp"]["enabled"] = json!(true);
    value["mcp"]["mode"] = json!("ask");
    value["mcp"]["port"] = json!(port);
    let saved = headless(&workspace, "settings")
        .action("save")
        .request(value)
        .finish();
    report.record(&saved);
    saved.expect_ok();
    assert_eq!(saved.data()["mcp"]["enabled"], true);
    assert_eq!(saved.data()["mcp"]["port"], port);

    let status = headless(&workspace, "mcp").action("status").finish();
    report.record(&status);
    status.expect_ok();
    assert_eq!(status.data()["enabled"], true, "{status:?}");
    assert_eq!(status.data()["running"], false, "{status:?}");
    assert_eq!(status.data()["port"], port, "{status:?}");
    assert!(
        status.data()["error"]
            .as_str()
            .unwrap_or_default()
            .contains("无法监听"),
        "端口被占用时状态要说明原因：{status:?}"
    );
    assert!(
        !workspace.app_root().join("mcp-runtime.json").is_file(),
        "启动失败不能留下运行时文件"
    );

    let reread = headless(&workspace, "settings").action("get").finish();
    report.record(&reread);
    reread.expect_ok();
    assert_eq!(reread.data()["mcp"]["enabled"], true);
    assert_eq!(reread.data()["mcp"]["port"], port);
    drop(listener);

    workspace.assert_within_budget();
    report.success();
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("无法取空闲端口");
    listener.local_addr().unwrap().port()
}

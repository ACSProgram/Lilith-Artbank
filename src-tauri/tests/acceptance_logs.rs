#![cfg(feature = "headless")]

mod acceptance_support;

use std::{fs, path::PathBuf};

use chrono::{Local, NaiveDateTime};
use serde_json::json;

use acceptance_support::{Report, Tier, Workspace, headless, read_log, skip_unless};

#[test]
fn startup_banner_has_local_time_and_run_marker() {
    if skip_unless(Tier::Cheap, "startup_banner_has_local_time_and_run_marker") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "startup_banner_has_local_time_and_run_marker",
        Tier::Cheap,
        &workspace,
    );

    let path = headless(&workspace, "logs").action("path").finish();
    report.record(&path);
    path.expect_ok();
    assert_eq!(
        PathBuf::from(path.data()["logPath"].as_str().unwrap()),
        workspace.log_file()
    );

    let log = read_log(&workspace);
    let line = log
        .lines()
        .find(|line| line.contains("Lilith Artbank starting"))
        .unwrap_or_else(|| panic!("缺少启动横幅：\n{log}"));

    assert_eq!(&line[4..5], "-");
    assert_eq!(&line[7..8], "-");
    assert_eq!(&line[10..11], " ");
    assert_eq!(&line[13..14], ":");
    assert_eq!(&line[16..17], ":");
    assert_eq!(&line[19..24], "[run:");
    let run = &line[24..30];
    assert!(
        run.chars().all(|ch| ch.is_ascii_hexdigit()),
        "运行标识不是 6 位十六进制：{run}"
    );
    assert_eq!(&line[30..31], "]");
    assert!(line[31..].starts_with("[INFO]["));
    let target_start = 31 + "[INFO][".len();
    let target_end = line[target_start..]
        .find("] ")
        .map(|offset| target_start + offset)
        .expect("缺少 target 括号");
    assert!(target_end > target_start, "target 不能为空");
    let message = &line[target_end + 2..];
    assert!(message.contains("Lilith Artbank starting:"));
    assert!(message.contains("version="));
    assert!(message.contains(&format!("root={}", workspace.app_root().display())));
    assert!(message.contains("run="));
    assert!(message.contains("level=debug"));
    assert!(message.contains(&format!("log_dir={}", workspace.logs_dir().display())));
    assert!(message.contains("rotation=4MB×5"));

    let stamp = NaiveDateTime::parse_from_str(&line[..19], "%Y-%m-%d %H:%M:%S")
        .unwrap_or_else(|error| panic!("行时间格式不对：{error}\n{line}"));
    let delta = (Local::now().naive_local() - stamp).num_seconds().abs();
    assert!(delta < 300, "行时间不是本地时间，相差 {delta} 秒");

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn command_records_write_pairs_failures_and_silent_reads() {
    if skip_unless(
        Tier::Cheap,
        "command_records_write_pairs_failures_and_silent_reads",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "command_records_write_pairs_failures_and_silent_reads",
        Tier::Cheap,
        &workspace,
    );
    let library = workspace.library();

    let created = headless(&workspace, "library")
        .action("create")
        .request(json!({ "path": library.to_string_lossy(), "name": "日志库" }))
        .finish();
    report.record(&created);
    created.expect_ok();
    assert_eq!(created.stdout.trim().lines().count(), 1);

    let read = headless(&workspace, "query")
        .action("stats")
        .library(&library)
        .request(json!({ "profile": "summary", "query": {} }))
        .finish();
    report.record(&read);
    read.expect_ok();

    let plain = workspace.out("plain");
    fs::create_dir_all(&plain).unwrap();
    let failed = headless(&workspace, "library")
        .action("open")
        .request(json!({ "path": plain.to_string_lossy() }))
        .finish();
    report.record(&failed);
    failed.expect_failed("目录不是图库");

    let log = read_log(&workspace);
    for needle in [
        "command started: name=create_library",
        "command finished: name=create_library",
        "command started: name=open_library_path",
        "command finished: name=open_library_path",
        "command failed: name=open_library_path",
        "目录不是图库",
    ] {
        assert!(log.contains(needle), "日志缺少 {needle:?}：\n{log}");
    }
    assert!(
        !log.contains("name=get_library_stats"),
        "成功且快速的读命令不应落盘：\n{log}"
    );

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn level_filter_controls_debug_and_info() {
    if skip_unless(Tier::Cheap, "level_filter_controls_debug_and_info") {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "level_filter_controls_debug_and_info",
        Tier::Cheap,
        &workspace,
    );

    let info = headless(&workspace, "logs")
        .action("emit")
        .env("LILITH_ARTBANK_LOG_LEVEL", "warn")
        .request(json!({ "level": "info", "lines": 1, "lineBytes": 8 }))
        .finish();
    report.record(&info);
    info.expect_ok();
    let warn = headless(&workspace, "logs")
        .action("emit")
        .env("LILITH_ARTBANK_LOG_LEVEL", "warn")
        .request(json!({ "level": "warn", "lines": 1, "lineBytes": 8 }))
        .finish();
    report.record(&warn);
    warn.expect_ok();

    let log = read_log(&workspace);
    assert!(log.contains("[WARN][acceptance]"), "缺少 warn 行：\n{log}");
    assert!(!log.contains("[INFO]"), "warn 档不应出现 info：\n{log}");
    assert!(!log.contains("[DEBUG]"), "warn 档不应出现 debug：\n{log}");

    let debug = headless(&workspace, "logs")
        .action("emit")
        .request(json!({ "level": "debug", "lines": 2, "lineBytes": 8 }))
        .finish();
    report.record(&debug);
    debug.expect_ok();
    let log = read_log(&workspace);
    assert!(
        log.contains("[DEBUG][acceptance]"),
        "调试构建默认应出现 debug：\n{log}"
    );
    assert!(log.contains("Lilith Artbank starting"));

    workspace.assert_within_budget();
    report.success();
}

#[test]
fn rotates_after_four_megabytes_and_keeps_five_archives() {
    if skip_unless(
        Tier::Default,
        "rotates_after_four_megabytes_and_keeps_five_archives",
    ) {
        return;
    }
    let workspace = Workspace::new();
    let mut report = Report::new(
        "rotates_after_four_megabytes_and_keeps_five_archives",
        Tier::Default,
        &workspace,
    );
    let logs = workspace.logs_dir();
    fs::create_dir_all(&logs).unwrap();
    for index in 0..7 {
        fs::write(
            logs.join(format!("lilith-artbank_2026-01-01_00-00-0{index}.log")),
            b"old\n",
        )
        .unwrap();
    }

    let path = headless(&workspace, "logs").action("path").finish();
    report.record(&path);
    path.expect_ok();
    assert_eq!(archives(&workspace).len(), 5, "初始化应先裁到 5 份归档");

    let emitted = headless(&workspace, "logs")
        .action("emit")
        .request(json!({ "level": "info", "lines": 1000, "lineBytes": 5000 }))
        .finish();
    report.record(&emitted);
    emitted.expect_ok();

    let archives = archives(&workspace);
    assert_eq!(archives.len(), 5, "轮转后归档仍应只有 5 份");
    let archived: u64 = archives
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum();
    assert!(archived >= 3_900_000, "至少应有一份接近 4MB 的归档");
    let active = fs::metadata(workspace.log_file()).unwrap().len();
    assert!(
        active > 0 && active < 4_100_000,
        "活动日志应在轮转后变小：{active}"
    );
    let log_files = fs::read_dir(&logs)
        .unwrap()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("lilith-artbank") && name.ends_with(".log")
        })
        .count();
    assert_eq!(log_files, 6, "活动文件 + 5 份归档");

    workspace.assert_within_budget();
    report.success();
}

fn archives(workspace: &Workspace) -> Vec<PathBuf> {
    fs::read_dir(workspace.logs_dir())
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    let name = path.file_name().unwrap().to_string_lossy();
                    name.starts_with("lilith-artbank_") && name.ends_with(".log")
                })
                .collect()
        })
        .unwrap_or_default()
}

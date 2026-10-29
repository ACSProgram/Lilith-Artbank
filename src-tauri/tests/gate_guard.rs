//! 门控守护：`tests/` 顶层会启动无头子进程的测试必须带 `#![cfg(feature = "headless")]`。
//!
//! Cargo 会自动发现 `tests/` 下的每一个 `.rs` 文件并作为独立目标编译运行；会拉起子进程
//! 的目标若不带门控，裸 `cargo test` 就会启动应用（`--headless` 被忽略）并挂住。
//! 本文件自身不带门控：它必须在裸 `cargo test` 下运行，且只读目录，不启动进程。

use std::{fs, path::PathBuf};

const REQUIRED_GATE: &str = "#![cfg(feature = \"headless\")]";
const SPAWN_MARKERS: [&str; 3] = ["CARGO_BIN_EXE_", "mod acceptance_support;", "Command::new("];

fn has_gate(source: &str) -> bool {
    source.lines().any(|line| line.trim() == REQUIRED_GATE)
}

fn spawns_process(source: &str) -> bool {
    SPAWN_MARKERS.iter().any(|marker| source.contains(marker))
}

fn tests_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn test_targets() -> Vec<(String, PathBuf)> {
    let mut targets = Vec::new();
    let entries = fs::read_dir(tests_directory()).expect("无法读取 tests 目录");
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".rs") && name != "gate_guard.rs" {
            targets.push((name, path));
        }
    }
    targets.sort();
    targets
}

#[test]
fn every_process_spawning_test_target_is_gated_behind_the_headless_feature() {
    let targets = test_targets();
    assert!(
        !targets.is_empty(),
        "未在 {} 找到任何测试目标——守护测试自身可能失效",
        tests_directory().display()
    );

    let mut missing = Vec::new();
    for (name, path) in &targets {
        let source = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("无法读取 {}：{error}", path.display()));
        if spawns_process(&source) && !has_gate(&source) {
            missing.push(name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "以下测试会启动子进程但没有门控行 {REQUIRED_GATE}：{missing:?}"
    );
}

/// 不带门控的文件不得出现启动子进程的标记；否则它会在裸 `cargo test` 下拉起应用。
#[test]
fn ungated_targets_do_not_spawn_processes() {
    for (name, path) in test_targets() {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("无法读取 {}：{error}", path.display()));
        assert!(
            has_gate(&source) || !spawns_process(&source),
            "{name} 没有门控却会启动子进程；加门控或移除启动逻辑"
        );
    }
}

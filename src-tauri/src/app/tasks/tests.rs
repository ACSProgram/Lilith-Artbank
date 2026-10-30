use std::time::Duration;

use crate::concurrency::resources::{HolderInfo, ResourceRequest};

use super::*;

#[test]
fn resource_admission_replaces_lane_conflicts() {
    let bus = TaskBus::new();
    let holder = HolderInfo::new("reader", "生成描述");
    let reader = ResourceRequest::session_read()
        .with_ai_service()
        .with_content_writer()
        .with_metadata_writer();
    let _guard = bus.acquire_immediate(&reader, &holder).unwrap();
    // 内容写者与 AI 互斥：编辑/压缩替换内容时不能让 AI 读旧缩略图。
    let error = bus
        .acquire_immediate(
            &ResourceRequest::session_read().with_content_writer(),
            &HolderInfo::new("edit", "保存编辑"),
        )
        .unwrap_err();
    assert!(error.to_string().contains("生成描述"));
    // 文件夹维护只需要路径写，不和 AI/内容写冲突。
    let folder = bus
        .acquire_immediate(
            &ResourceRequest::session_read()
                .with_timeline(crate::concurrency::resources::Access::Read)
                .with_paths(vec![crate::concurrency::resources::PathScope::write(
                    "角色",
                )]),
            &HolderInfo::new("folder", "重命名文件夹"),
        )
        .unwrap();
    drop(folder);
    // 另一个 AI 任务仍然被 ai.service 挡住。
    let error = bus
        .acquire_immediate(
            &ResourceRequest::session_read()
                .with_ai_service()
                .with_content_writer(),
            &HolderInfo::new("other", "生成姿势"),
        )
        .unwrap_err();
    assert!(error.to_string().contains("生成描述"));
}

#[test]
fn handle_snapshot_reflects_progress() {
    let handle = TaskHandle::new("t1", TaskKind::Thumbnail, "选中 2 张");
    handle.set_total(2);
    handle.inc_completed();
    handle.inc_failed();
    handle.set_current(Some(7));
    handle.add_failure(Some(8), "", "无法解码");
    handle.set_phase(TaskPhase::Running);
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.total, 2);
    assert_eq!(snapshot.completed, 1);
    assert_eq!(snapshot.failed, 1);
    assert_eq!(snapshot.current_image_id, Some(7));
    assert_eq!(snapshot.failures.len(), 1);
    assert!(!snapshot.phase.is_terminal());

    handle.finish(TaskPhase::Completed);
    let snapshot = handle.snapshot();
    assert!(snapshot.phase.is_terminal());
    assert_eq!(snapshot.current_image_id, None);
}

#[test]
fn scopes_serialize_with_camel_case_fields() {
    let scope: TaskScope =
        serde_json::from_str(r#"{"type": "folder", "folderId": 4, "includeSubfolders": true}"#)
            .unwrap();
    assert!(matches!(
        scope,
        TaskScope::Folder {
            folder_id: 4,
            include_subfolders: true
        }
    ));
    let scope: TaskScope =
        serde_json::from_str(r#"{"type": "selected", "imageIds": [3, 1]}"#).unwrap();
    assert_eq!(scope.label(), "选中 2 张");
    let scope: TaskScope = serde_json::from_str(r#"{"type": "library"}"#).unwrap();
    assert_eq!(scope.label(), "整个图库");
}

#[test]
fn task_start_result_is_tagged() {
    let value = serde_json::to_value(TaskStartResult::Conflict {
        active: vec![TaskSummary {
            task_id: "a".into(),
            kind: TaskKind::Import,
            label: "导入".into(),
            scope: "整个图库".into(),
            phase: TaskPhase::Running,
        }],
    })
    .unwrap();
    assert_eq!(value["status"], "conflict");
    assert_eq!(value["active"][0]["taskId"], "a");
}

#[test]
fn non_cancellable_tasks_ignore_cancel_requests() {
    let bus = TaskBus::new();
    let handle = TaskHandle::new("u1", TaskKind::Undo, "撤销 1 条记录");
    handle.set_cancellable(false);
    handle.set_phase(TaskPhase::Running);
    bus.register(handle.clone());
    assert!(bus.cancel("u1").is_err());
    assert!(!handle.should_cancel());
    assert!(!handle.snapshot().cancellable);

    handle.set_cancellable(true);
    bus.cancel("u1").unwrap();
    assert!(handle.should_cancel());
    assert!(handle.snapshot().cancellable);
}

#[test]
fn cancel_request_marks_phase_cancelling_until_terminal() {
    let handle = TaskHandle::new("c1", TaskKind::Thumbnail, "选中 3 张");
    handle.set_phase(TaskPhase::Running);
    handle.request_cancel();
    assert!(handle.should_cancel());
    assert_eq!(handle.phase(), TaskPhase::Cancelling);
    // 取消中不是终态：任务仍在跑，前端要继续轮询进度。
    assert!(!TaskPhase::Cancelling.is_terminal());
    assert!(handle.is_running());

    handle.finish(TaskPhase::Cancelled);
    handle.request_cancel();
    assert_eq!(
        handle.phase(),
        TaskPhase::Cancelled,
        "终态之后相位不应再被改写"
    );
}

#[test]
fn undo_and_release_have_labels() {
    assert_eq!(TaskKind::Undo.label(), "撤销操作");
    assert_eq!(TaskKind::ReleaseGarbage.label(), "释放删除文件");
}

fn wait_terminal(handle: &TaskHandle) {
    for _ in 0..200 {
        if handle.is_finished() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("任务没有在预期时间内结束");
}

#[test]
fn panicking_worker_finishes_failed_and_releases_resources() {
    let bus = TaskBus::new();
    let guard = bus
        .acquire_immediate(
            &ResourceRequest::session_read(),
            &HolderInfo::new("panic-holder", "测试"),
        )
        .unwrap();
    let handle = TaskHandle::new("panic", TaskKind::Thumbnail, "选中 1 张");
    let worker = handle.clone();
    bus.spawn_worker("panic-test".to_owned(), handle.clone(), move || {
        let _guard = guard;
        panic!("boom");
    })
    .unwrap();
    wait_terminal(&handle);
    assert_eq!(worker.phase(), TaskPhase::Failed);
    assert!(worker.snapshot().message.contains("boom"));
    assert!(bus.is_idle());
}

#[test]
fn worker_returning_without_terminal_is_marked_failed() {
    let bus = TaskBus::new();
    let handle = TaskHandle::new("early", TaskKind::Thumbnail, "选中 1 张");
    bus.spawn_worker("early-return".to_owned(), handle.clone(), || {})
        .unwrap();
    wait_terminal(&handle);
    assert_eq!(handle.phase(), TaskPhase::Failed);
    assert!(handle.snapshot().message.contains("未报告终态"));
}

#[test]
fn snapshot_reports_run_time_rate_and_eta() {
    let handle = TaskHandle::new("timed", TaskKind::Thumbnail, "选中 10 张");
    assert!(handle.snapshot().run_started_at.is_none());
    assert!(handle.snapshot().rate_per_second.is_none());
    assert!(handle.snapshot().eta_seconds.is_none());

    handle.set_total(10);
    handle.set_phase(TaskPhase::Queued);
    assert!(
        handle.snapshot().run_started_at.is_none(),
        "排队时间不应算进运行时长"
    );
    handle.set_phase(TaskPhase::Running);
    assert!(handle.snapshot().run_started_at.is_some());

    handle.set_completed(2);
    handle.set_current_file(Some("人物/a.jpg"));
    std::thread::sleep(Duration::from_millis(600));
    let snapshot = handle.snapshot();
    assert!(snapshot.rate_per_second.is_some());
    assert!(snapshot.eta_seconds.is_some());
    assert_eq!(snapshot.current_file.as_deref(), Some("人物/a.jpg"));

    handle.finish(TaskPhase::Completed);
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.current_file, None);
    assert_eq!(snapshot.eta_seconds, None);
}

/// 全部跳过时速度同样有值：跳过的图也消耗了完整的工作量。
#[test]
fn rate_counts_skipped_work() {
    let handle = TaskHandle::new("skips", TaskKind::Optimize, "选中 10 张");
    handle.set_total(10);
    handle.set_phase(TaskPhase::Running);
    handle.set_skipped(4);
    std::thread::sleep(Duration::from_millis(600));
    let snapshot = handle.snapshot();
    assert!(snapshot.completed == 0 && snapshot.failed == 0);
    let rate = snapshot.rate_per_second.expect("全跳过时速度不能为空");
    assert!(rate > 0.0, "{rate}");
    // ETA 与速度同源：还剩 6 张没结算。
    assert!(snapshot.eta_seconds.is_some());
}

/// 压缩不暴露计量器加权百分比，四个位置读同一个「已结算 / 候选张数」。
#[test]
fn optimize_snapshot_percent_comes_from_counters() {
    let handle = TaskHandle::new("opt", TaskKind::Optimize, "整个图库");
    handle.set_total(100);
    handle.set_completed(40);
    handle.set_skipped(22);
    handle.set_failed(2);
    handle.set_percent(73);
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.progress_percent, None, "压缩不该带加权百分比");

    // 其他任务仍用计量器的阶段加权。
    let other = TaskHandle::new("imp", TaskKind::Import, "选中 100 张");
    other.set_total(100);
    other.set_percent(73);
    assert_eq!(other.snapshot().progress_percent, Some(73));
}

/// 跳过细分整体替换，前一批的桶不会残留。
#[test]
fn skip_breakdown_replaces_previous_buckets() {
    let handle = TaskHandle::new("buckets", TaskKind::Optimize, "整个图库");
    handle.set_skip_breakdown(&[("quality".to_owned(), 3), ("depth".to_owned(), 1)]);
    assert_eq!(handle.snapshot().skip_breakdown.len(), 2);
    handle.set_skip_breakdown(&[("notSmaller".to_owned(), 7)]);
    let breakdown = handle.snapshot().skip_breakdown;
    assert_eq!(breakdown.len(), 1);
    assert_eq!(
        (breakdown[0].key.as_str(), breakdown[0].count),
        ("notSmaller", 7)
    );
}

/// 让位是终态：用户没点取消，胶囊上是「已让位」而不是「已取消」。
#[test]
fn yielded_is_terminal_and_not_retried() {
    let handle = TaskHandle::new("y1", TaskKind::PackCompact, "空闲回收");
    handle.set_phase(TaskPhase::Running);
    handle.set_message("已让位 · 已回收 1 个段");
    handle.finish(TaskPhase::Yielded);
    let snapshot = handle.snapshot();
    assert!(snapshot.phase.is_terminal());
    assert_eq!(snapshot.phase, TaskPhase::Yielded);
    assert!(handle.is_finished());
    assert_eq!(TaskKind::PackCompact.label(), "回收缩略图包");

    // 终态之后取消请求不再改写相位。
    handle.request_cancel();
    assert_eq!(handle.phase(), TaskPhase::Yielded);
    // 让位的任务不参与「还在跑」的判断，排队中的下一个可以启动。
    let bus = TaskBus::new();
    bus.register(handle.clone());
    assert!(bus.is_idle());
}

#[test]
fn maintenance_task_kinds_have_labels() {
    assert_eq!(TaskKind::CheckLibrary.label(), "检查图库");
    assert_eq!(TaskKind::Export.label(), "导出图片");
    assert_eq!(TaskKind::OrphanMove.label(), "清理孤儿文件");
    // 关库时不阻塞的：检查、导出、回收与释放；孤儿移动要等它做完。
    assert!(TaskKind::CheckLibrary.can_cancel_for_close());
    assert!(TaskKind::Export.can_cancel_for_close());
    assert!(TaskKind::PackCompact.can_cancel_for_close());
    assert!(TaskKind::ReleaseGarbage.can_cancel_for_close());
    assert!(!TaskKind::OrphanMove.can_cancel_for_close());
}

#[test]
fn history_trim_keeps_running_tasks() {
    let bus = TaskBus::new();
    let running = TaskHandle::new("running", TaskKind::Import, "运行中");
    running.set_phase(TaskPhase::Running);
    bus.register(running.clone());
    for index in 0..60 {
        let handle = TaskHandle::new(format!("done-{index}"), TaskKind::Import, "已完成");
        handle.finish(TaskPhase::Completed);
        bus.register(handle);
    }
    let tasks = bus.list();
    assert!(tasks.iter().any(|task| task.task_id == "running"));
    assert_eq!(tasks.len(), MAX_TASKS);
}

/// 排队期间换库（关库/开库）：拿到资源时按代次取消，不再碰旧库。
#[test]
fn a_queued_task_is_dropped_when_the_library_epoch_moves() {
    let bus = TaskBus::new();
    let handle = TaskHandle::new("queued", TaskKind::Thumbnail, "选中 3 张");
    let stamp = bus.epoch();
    assert!(bus.admit_queued(&handle, stamp).is_ok());

    bus.bump_epoch();
    let refused = bus.admit_queued(&handle, stamp);
    assert_eq!(refused, Err(Some("图库已切换，任务未开始")));

    // 新提交的任务拿到的是新代次，不受影响。
    let fresh = bus.epoch();
    assert!(bus.admit_queued(&handle, fresh).is_ok());
}

/// 取消优先于代次：用户点过取消的任务不该被写成「图库已切换」。
#[test]
fn a_cancelled_queued_task_keeps_its_own_message() {
    let bus = TaskBus::new();
    let handle = TaskHandle::new("queued", TaskKind::Thumbnail, "选中 3 张");
    let stamp = bus.epoch();
    bus.bump_epoch();
    handle.request_cancel();
    assert_eq!(bus.admit_queued(&handle, stamp), Err(None));
}

/// 部分失败是终态并保留说明；取消按钮不再出现。
#[test]
fn partial_failure_is_terminal_and_keeps_the_message() {
    let handle = TaskHandle::new("partial", TaskKind::PackCompact, "立即回收");
    handle.set_phase(TaskPhase::Running);
    handle.finish_partial("已回收 1 个段 · 释放 12 MB · 2 个旧段删除失败");
    let snapshot = handle.snapshot();
    assert!(snapshot.phase.is_terminal());
    assert_eq!(snapshot.phase, TaskPhase::PartialFailed);
    assert!(snapshot.message.contains("删除失败"));
    assert!(handle.is_finished());
}

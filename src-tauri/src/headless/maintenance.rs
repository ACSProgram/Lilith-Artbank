//! 任务与维护命令，以及需要同进程编排的场景。
//!
//! 场景只做编排：启动任务、取消、关闭、持锁与等待，把事实写进结果 JSON；
//! 对错由 `tests/` 断言。

use std::{
    cell::Cell,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::Manager;

use crate::{
    app::{
        activity,
        error::AppError,
        state::AppState,
        tasks::{
            BatchTaskKind, BatchTaskRequest, ConflictPolicy, LaneGuard, TaskHandle, TaskKind,
            TaskPhase, TaskScope, TaskSnapshot, TaskStartResult,
        },
        trace,
    },
    import::ImportRequest,
    library::query::LibraryQuery,
};

use super::{NullReporter, Options, check_task, value_of, wait_task, wait_task_with_timeout};

pub(super) fn dispatch(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let state = state.inner();
    match (options.command.as_str(), options.action.as_deref()) {
        ("task", Some("list")) => Ok(json!({ "tasks": state.tasks_snapshot() })),
        ("task", Some("wait")) => {
            let request: WaitRequest = options.request_or_default()?;
            let timeout = Duration::from_millis(request.timeout_ms.max(100));
            match request.task_id.as_deref() {
                Some(task_id) => {
                    let snapshot = wait_task_with_timeout(state, task_id, timeout)?;
                    check_task(&snapshot, cancelled)?;
                    Ok(json!({ "tasks": [snapshot] }))
                }
                None => {
                    if !state.tasks.wait_idle(timeout) {
                        return Err("等待任务结束超时".to_owned());
                    }
                    Ok(json!({ "tasks": state.tasks_snapshot() }))
                }
            }
        }
        ("task", Some("cancel")) => {
            let request: TaskIdRequest = options.request()?;
            trace::write("cancel_task", || {
                AppError::command(state.cancel_task(&request.task_id))
            })?;
            Ok(json!({ "taskId": request.task_id, "cancelRequested": true }))
        }
        ("maintenance", Some("thumbnails")) => {
            let request: ThumbnailRequest = options.request_or_default()?;
            let task = run_batch(
                app,
                BatchTaskRequest {
                    kind: BatchTaskKind::Thumbnail,
                    scope: request.scope,
                    overwrite: request.overwrite,
                    conflict: ConflictPolicy::Fail,
                    ..BatchTaskRequest::default()
                },
                cancelled,
            )?;
            Ok(json!({ "task": task }))
        }
        ("maintenance", Some("check")) => {
            let request: CheckRequest = options.request_or_default()?;
            let kind = match request.mode.trim().to_ascii_lowercase().as_str() {
                "fast" => BatchTaskKind::CheckFast,
                "full" => BatchTaskKind::CheckFull,
                other => return Err(format!("检查模式只能是 fast/full，收到 {other:?}")),
            };
            let task = run_batch(
                app,
                BatchTaskRequest {
                    kind,
                    scope: request.scope,
                    conflict: ConflictPolicy::Fail,
                    ..BatchTaskRequest::default()
                },
                cancelled,
            )?;
            // 报告按任务 id 挂在状态里，任务一结束就能取；已被新检查覆盖时为 null。
            let report = state.inspection_report(&task.task_id);
            Ok(json!({ "task": task, "report": report }))
        }
        ("maintenance", Some("export")) => {
            let request: ExportRequest = options.request()?;
            let task = run_batch(
                app,
                BatchTaskRequest {
                    kind: BatchTaskKind::Export,
                    scope: request.scope,
                    target_dir: Some(request.target_dir),
                    include_manifest: request.include_manifest,
                    conflict: request.conflict,
                    ..BatchTaskRequest::default()
                },
                cancelled,
            )?;
            Ok(json!({ "task": task }))
        }
        ("maintenance", Some("orphan-move")) => {
            let request: OrphanMoveRequest = options.request()?;
            let start = trace::write("move_orphans", || {
                AppError::command(state.move_orphans_task(
                    app.clone(),
                    &request.report_task_id,
                    request.paths,
                ))
            })?;
            let task = started_task(start)?;
            let snapshot = wait_task(state, &task.task_id)?;
            check_task(&snapshot, cancelled)?;
            Ok(json!({ "task": snapshot }))
        }
        ("maintenance", Some("save-report")) => {
            let request: SaveReportRequest = options.request()?;
            value_of(trace::write("save_inspection_report", || {
                AppError::command(state.save_inspection_report(
                    &request.task_id,
                    std::path::Path::new(&request.path),
                    request.group.as_deref(),
                ))
            }))
            .map(|_| json!({ "saved": true }))
        }
        ("maintenance", Some("pack")) => {
            let stats = trace::read("get_pack_stats", || AppError::command(state.pack_stats()))?;
            let last = AppError::command(state.last_pack_compact())?;
            Ok(json!({
                "stats": stats,
                "lastCompact": last,
                "triggerRatio": crate::library::pack_gc::TRIGGER_RATIO,
            }))
        }
        ("maintenance", Some("compact")) => {
            let start = trace::write("start_pack_compact", || {
                AppError::command(state.compact_pack_now(app.clone()))
            })?;
            let task = started_task(start)?;
            let snapshot = wait_task(state, &task.task_id)?;
            check_task(&snapshot, cancelled)?;
            Ok(json!({
                "task": snapshot,
                "lastCompact": AppError::command(state.last_pack_compact())?,
            }))
        }
        ("maintenance", Some("zero-tags")) => value_of(trace::write("delete_zero_tags", || {
            AppError::command(state.delete_zero_tags())
        })),
        ("scenario", Some(name)) => run_scenario(app, name, options, cancelled),
        ("task" | "maintenance" | "scenario", action) => Err(match action {
            Some(action) => format!("未知命令：{} {action}", options.command),
            None => format!("未知命令：{}", options.command),
        }),
        _ => unreachable!("mod.rs 只把本组命令交给这里"),
    }
}

/// 起一个批量任务并等到终态，返回终态快照。
pub(super) fn run_batch(
    app: &tauri::AppHandle,
    request: BatchTaskRequest,
    cancelled: &Cell<bool>,
) -> Result<TaskSnapshot, String> {
    run_batch_with_timeout(app, request, cancelled, super::TASK_TIMEOUT)
}

/// 同 [`run_batch`]，但允许长任务放宽等待上限。
pub(super) fn run_batch_with_timeout(
    app: &tauri::AppHandle,
    request: BatchTaskRequest,
    cancelled: &Cell<bool>,
    timeout: Duration,
) -> Result<TaskSnapshot, String> {
    let state = app.state::<AppState>();
    let start = trace::write("start_batch_task", || {
        AppError::command(state.start_batch_task(app.clone(), request))
    })?;
    let task = started_task(start)?;
    let snapshot = wait_task_with_timeout(&state, &task.task_id, timeout)?;
    check_task(&snapshot, cancelled)?;
    Ok(snapshot)
}

pub(super) fn started_task(start: TaskStartResult) -> Result<TaskSnapshot, String> {
    match start {
        TaskStartResult::Started { task } => Ok(*task),
        TaskStartResult::Conflict { active } => {
            let holder = active
                .first()
                .map(|task| format!("「{}」{}", task.label, task.scope))
                .unwrap_or_else(|| "其它任务".to_owned());
            Err(format!("{holder}正在进行，稍后重试"))
        }
    }
}

fn run_scenario(
    app: &tauri::AppHandle,
    name: &str,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    match name {
        "maintenance_check" => scenario_check(app, options, cancelled),
        "import_cancel" => scenario_import_cancel(app, options, cancelled),
        "task_queue_cancel" => scenario_task_queue_cancel(app, options, cancelled),
        "task_panic" => scenario_task_panic(app),
        "close_during_task" => scenario_close_during_task(app, options),
        "epoch_switch" => scenario_epoch_switch(app, options),
        "folder_lock" => scenario_folder_lock(app, options),
        "ai_lock" => scenario_ai_lock(app, options),
        "idle_gc" => scenario_idle_gc(app, options),
        "idle_yield" => scenario_idle_yield(app, options),
        "mcp_bridge" => super::mcp::scenario_bridge(app, options),
        "mcp_modes" => super::mcp::scenario_modes(app, options),
        "optimize_flow" => super::optimize::scenario(app, options, cancelled),
        "ai_search_flow" => super::ai::scenario(app, options),
        "ai_temp_flow" => super::ai::temp_scenario(app, options, cancelled),
        other => Err(format!("未知场景：{other}")),
    }
}

/// 起一个持锁任务：立即拿到资源，在后台线程睡 `hold` 后自己收尾。
fn hold_task(
    app: &tauri::AppHandle,
    kind: TaskKind,
    scope: &str,
    request: crate::concurrency::resources::ResourceRequest,
    hold: Duration,
) -> Result<Arc<TaskHandle>, String> {
    let state = app.state::<AppState>();
    let handle = TaskHandle::new(uuid::Uuid::new_v4().to_string(), kind, scope);
    let worker = handle.clone();
    let launched = state
        .tasks
        .launch(
            app.clone(),
            handle.clone(),
            request,
            ConflictPolicy::Fail,
            move |guard| {
                std::thread::spawn(move || {
                    let _guard: LaneGuard = guard;
                    std::thread::sleep(hold);
                    if !worker.is_finished() {
                        worker.finish(TaskPhase::Completed);
                    }
                });
            },
        )
        .map_err(|error| error.to_string())?;
    started_task(launched)?;
    Ok(handle)
}

fn wait_handle(state: &AppState, handle: &Arc<TaskHandle>) -> Result<TaskSnapshot, String> {
    wait_task_with_timeout(state, handle.id(), Duration::from_secs(30))
}

fn scenario_check(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let request: ScenarioCheckRequest = options.request_or_default()?;
    let kind = match request.mode.trim().to_ascii_lowercase().as_str() {
        "full" => BatchTaskKind::CheckFull,
        _ => BatchTaskKind::CheckFast,
    };
    let checked = run_batch(
        app,
        BatchTaskRequest {
            kind,
            scope: request.scope,
            conflict: ConflictPolicy::Fail,
            ..BatchTaskRequest::default()
        },
        cancelled,
    )?;
    let report = state
        .inspection_report(&checked.task_id)
        .ok_or_else(|| "检查报告已被新的检查覆盖".to_owned())?;
    let mut saved = false;
    if let Some(path) = request.save_report_path.as_deref() {
        AppError::command(state.save_inspection_report(
            &checked.task_id,
            std::path::Path::new(path),
            request.save_group.as_deref(),
        ))?;
        saved = true;
    }
    let move_task = if request.move_orphans && !report.orphans.is_empty() {
        let paths: Vec<String> = report
            .orphans
            .iter()
            .map(|orphan| orphan.path.clone())
            .collect();
        let start = trace::write("move_orphans", || {
            AppError::command(state.move_orphans_task(app.clone(), &checked.task_id, paths))
        })?;
        let task = started_task(start)?;
        let snapshot = wait_task(&state, &task.task_id)?;
        check_task(&snapshot, cancelled)?;
        Some(snapshot)
    } else {
        None
    };
    Ok(json!({
        "task": checked,
        "report": report,
        "moveOrphans": move_task,
        "saved": saved,
    }))
}

fn scenario_import_cancel(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let request: ImportCancelRequest = options.request()?;
    let import: ImportRequest = serde_json::from_value(json!({
        "paths": request.paths,
        "target": request.target,
        "duplicatePolicy": "skip",
    }))
    .map_err(|error| format!("场景请求无效：{error}"))?;
    let snapshot = trace::write("start_import", || {
        AppError::command(state.start_import(app.clone(), import))
    })?;
    let job_id = snapshot.progress.job_id.clone();
    let refused = state.close_library().err().map(|error| error.to_string());
    wait_phase(
        &state,
        &job_id,
        |phase| matches!(phase, "running" | "processing" | "scanning"),
        Duration::from_secs(5),
    )?;
    std::thread::sleep(Duration::from_millis(request.cancel_after_ms.max(50)));
    AppError::command(state.cancel_import(&job_id))?;
    let task = wait_task(&state, &job_id)?;
    check_task(&task, cancelled)?;
    let job = state
        .import_receipt(&job_id)
        .ok_or_else(|| "取消后没有导入回执".to_owned())?;
    let counts_before = AppError::command(state.counts())?;
    let path = state
        .status()
        .map_err(|error| error.to_string())?
        .map(|info| info.path);
    let closed = state.close_library();
    let reopened = match (path, closed) {
        (Some(path), Ok(())) => state
            .open_library(std::path::Path::new(&path))
            .map(|_| AppError::command(state.counts()).ok())
            .map_err(|error| error.to_string())?,
        (_, Err(error)) => return Err(format!("取消后关闭图库失败：{error}")),
        (None, Ok(())) => None,
    };
    Ok(json!({
        "job": job,
        "task": task,
        "refusedCloseError": refused,
        "countsBeforeClose": counts_before,
        "reopened": reopened,
        "idle": state.tasks.wait_idle(Duration::from_secs(10)),
    }))
}

fn scenario_task_queue_cancel(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let request: QueueCancelRequest = options.request()?;
    let first: ImportRequest = serde_json::from_value(json!({
        "paths": request.paths,
        "duplicatePolicy": "skip",
    }))
    .map_err(|error| format!("场景请求无效：{error}"))?;
    let second = first.clone();
    let first_snapshot = trace::write("start_import", || {
        AppError::command(state.start_import(app.clone(), first))
    })?;
    let second_snapshot = trace::write("start_import", || {
        AppError::command(state.start_import(app.clone(), second))
    })?;
    let first_id = first_snapshot.progress.job_id.clone();
    let second_id = second_snapshot.progress.job_id.clone();
    wait_phase(
        &state,
        &second_id,
        |phase| phase == "queued",
        Duration::from_secs(5),
    )?;
    let second_queued = task_phase(&state, &second_id);
    AppError::command(state.cancel_import(&second_id))?;
    let second_cancelled = task_phase(&state, &second_id);
    AppError::command(state.cancel_import(&first_id))?;
    let first_snapshot_after = wait_task(&state, &first_id)?;
    check_task(&first_snapshot_after, cancelled)?;
    let second_snapshot_after = wait_task(&state, &second_id)?;
    check_task(&second_snapshot_after, cancelled)?;
    let idle = state.tasks.wait_idle(Duration::from_secs(30));
    Ok(json!({
        "first": first_snapshot_after,
        "second": second_snapshot_after,
        "secondPhaseWhileQueued": second_queued,
        "secondPhaseAfterCancel": second_cancelled,
        "counts": AppError::command(state.counts())?,
        "idle": idle,
    }))
}

fn wait_phase(
    state: &AppState,
    task_id: &str,
    predicate: impl Fn(&str) -> bool,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let phase = task_phase(state, task_id);
        if predicate(phase.as_deref().unwrap_or_default()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("任务 {task_id} 没有进入期望阶段，最后是 {phase:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn task_phase(state: &AppState, task_id: &str) -> Option<String> {
    state
        .tasks_snapshot()
        .into_iter()
        .find(|snapshot| snapshot.task_id == task_id)
        .map(|snapshot| format!("{:?}", snapshot.phase).to_ascii_lowercase())
}

fn scenario_task_panic(app: &tauri::AppHandle) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let handle = TaskHandle::new(
        uuid::Uuid::new_v4().to_string(),
        TaskKind::Thumbnail,
        "验收 panic",
    );
    let start = state
        .tasks
        .launch(
            app.clone(),
            handle.clone(),
            crate::concurrency::resources::ResourceRequest::session_read().with_content_writer(),
            ConflictPolicy::Fail,
            move |guard| {
                let _guard: LaneGuard = guard;
                panic!("验收注入的任务 panic");
            },
        )
        .map_err(|error| error.to_string())?;
    started_task(start)?;
    let failed = wait_handle(&state, &handle)?;
    // 后续任务必须能拿到资源，证明 panic 路径释放了守卫。
    let follow = hold_task(
        app,
        TaskKind::Thumbnail,
        "panic 后复查",
        crate::concurrency::resources::ResourceRequest::session_read().with_content_writer(),
        Duration::from_millis(10),
    )?;
    let follow = wait_handle(&state, &follow)?;
    Ok(json!({
        "failed": failed,
        "followUp": follow,
        "idle": state.tasks.wait_idle(Duration::from_secs(10)),
    }))
}

/// 排队任务在换库后拿到资源时按代次取消：这是一行都没写的安全取消。
fn scenario_epoch_switch(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let request: HoldRequest = options.request_or_default()?;
    let hold = hold_task(
        app,
        TaskKind::Thumbnail,
        "验收占位内容写",
        crate::concurrency::resources::ResourceRequest::session_read()
            .with_content_writer()
            .with_paths(vec![crate::concurrency::resources::PathScope::write("")]),
        Duration::from_millis(request.hold_ms.max(200)),
    )?;
    // 内容写被占住，回收任务只能排队。
    let queued = started_task(trace::write("start_pack_compact", || {
        AppError::command(state.start_pack_compact(app.clone()))
    })?)?;
    wait_phase(
        state.inner(),
        &queued.task_id,
        |phase| phase == "queued",
        Duration::from_secs(5),
    )?;
    // 等价于「关库把代次推进了一代」；排队者还没写任何东西。
    state.tasks.bump_epoch();
    let queued_after = wait_task(state.inner(), &queued.task_id)?;
    let holder_after = wait_handle(state.inner(), &hold)?;
    let closed = state.close_library();
    let still_open = state.status().map_err(|error| error.to_string())?.is_some();
    Ok(json!({
        "queued": queued_after,
        "holder": holder_after,
        "closed": closed.is_ok(),
        "closeError": closed.err().map(|error| error.to_string()),
        "libraryStillOpen": still_open,
        "idle": state.tasks.wait_idle(Duration::from_secs(10)),
    }))
}

fn scenario_close_during_task(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let request: HoldRequest = options.request_or_default()?;
    let hold = hold_task(
        app,
        TaskKind::Import,
        "验收占位写任务",
        crate::concurrency::resources::ResourceRequest::session_read()
            .with_timeline(crate::concurrency::resources::Access::Read)
            .with_content_writer()
            .with_paths(vec![crate::concurrency::resources::PathScope::write("")]),
        Duration::from_millis(request.hold_ms.max(200)),
    )?;
    let refused = state.close_library().err().map(|error| error.to_string());
    let finished = wait_handle(state.inner(), &hold)?;
    let closed = state.close_library().is_ok();
    Ok(json!({
        "refusedError": refused,
        "task": finished,
        "closedAfterFinish": closed,
        "idle": state.tasks.is_idle(),
    }))
}

fn scenario_folder_lock(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let request: FolderLockRequest = options.request()?;
    let hold = hold_task(
        app,
        TaskKind::Import,
        &request.folder_path,
        crate::concurrency::resources::ResourceRequest::session_read().with_paths(vec![
            crate::concurrency::resources::PathScope::write(&request.folder_path),
        ]),
        Duration::from_millis(request.hold_ms.max(200)),
    )?;
    let blocked = state
        .rename_folder(request.folder_id, &request.new_name)
        .err()
        .map(|error| error.to_string());
    wait_handle(state.inner(), &hold)?;
    let renamed = state.rename_folder(request.folder_id, &request.new_name);
    let renamed_path = renamed.as_ref().ok().map(|change| {
        change
            .folders
            .iter()
            .find(|folder| folder.id == request.folder_id)
            .map(|folder| folder.path.clone())
    });
    Ok(json!({
        "blockedError": blocked,
        "renamed": renamed.is_ok(),
        "renamedPath": renamed_path,
        "error": renamed.err().map(|error| error.to_string()),
        "idle": state.tasks.wait_idle(Duration::from_secs(10)),
    }))
}

fn scenario_ai_lock(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let request: AiLockRequest = options.request()?;
    let hold = hold_task(
        app,
        TaskKind::AiDescription,
        "验收 AI 占位",
        crate::concurrency::resources::ResourceRequest::session_read()
            .with_ai_service()
            .with_content_writer(),
        Duration::from_millis(request.hold_ms.max(200)),
    )?;
    let metadata = state.update_image_score(request.image_id, 3);
    let content = state.tasks.acquire_immediate(
        &crate::concurrency::resources::ResourceRequest::session_read().with_content_writer(),
        &crate::concurrency::resources::HolderInfo::new("probe", "内容写复查"),
    );
    wait_handle(state.inner(), &hold)?;
    Ok(json!({
        "metadataWrite": metadata.is_ok(),
        "metadataError": metadata.err().map(|error| error.to_string()),
        "contentBlockedError": content.err().map(|error| error.to_string()),
        "idle": state.tasks.wait_idle(Duration::from_secs(10)),
    }))
}

fn scenario_idle_gc(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let request: IdleGcRequest = options.request_or_default()?;
    let (setup, before_ratio) = prepare_pack_garbage(app)?;
    let state = app.state::<AppState>();
    activity::touch();
    let (compacted, after_ratio) = wait_for_auto_compact(state.inner(), request.timeout_ms)?;
    Ok(json!({
        "thumbnails": setup["thumbnails"],
        "trashed": setup["trashed"],
        "deleted": setup["deleted"],
        "garbageBefore": before_ratio,
        "garbageAfter": after_ratio,
        "lastCompact": compacted,
        "idle": state.tasks.is_idle(),
    }))
}

fn scenario_idle_yield(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let request: IdleGcRequest = options.request_or_default()?;
    let (setup, before_ratio) = prepare_pack_garbage(app)?;
    let state = app.state::<AppState>();
    activity::touch();
    let start = trace::write("start_pack_compact", || {
        AppError::command(state.start_pack_compact(app.clone()))
    })?;
    let task = started_task(start)?;
    let yielded = wait_task(state.inner(), &task.task_id)?;
    // 让位后不再触碰活动时间，等空闲循环重新提交并真正压完。
    let (compacted, after_ratio) = wait_for_auto_compact(state.inner(), request.timeout_ms)?;
    Ok(json!({
        "thumbnails": setup["thumbnails"],
        "garbageBefore": before_ratio,
        "yielded": yielded,
        "garbageAfter": after_ratio,
        "lastCompact": compacted,
        "idle": state.tasks.is_idle(),
    }))
}

/// 造出缩略图包垃圾：先建缩略图，再删除全部行并清空记录，让段内只剩死数据。
fn prepare_pack_garbage(app: &tauri::AppHandle) -> Result<(Value, f64), String> {
    let state = app.state::<AppState>();
    activity::touch();
    let start = state
        .start_batch_task(
            app.clone(),
            BatchTaskRequest {
                kind: BatchTaskKind::Thumbnail,
                scope: TaskScope::Library,
                conflict: ConflictPolicy::Fail,
                ..BatchTaskRequest::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let thumbnail = started_task(start)?;
    let thumbnail = wait_task(state.inner(), &thumbnail.task_id)?;
    if thumbnail.phase == TaskPhase::Failed {
        return Err(format!("重建缩略图失败：{}", thumbnail.message));
    }

    let ids = state
        .query_image_ids(LibraryQuery::default())
        .map_err(|error| error.to_string())?;
    let trashed = state
        .trash_images(&ids)
        .map_err(|error| error.to_string())?;
    let deleted = state
        .delete_images(&ids, &NullReporter)
        .map_err(|error| error.to_string())?;
    let clear = state.clear_history().map_err(|error| error.to_string())?;
    let clear = started_task(clear)?;
    let clear = wait_task(state.inner(), &clear.task_id)?;
    if clear.phase == TaskPhase::Failed {
        return Err(format!("清空操作记录失败：{}", clear.message));
    }
    let before = state.pack_stats().map_err(|error| error.to_string())?;
    Ok((
        json!({
            "thumbnails": thumbnail,
            "trashed": trashed,
            "deleted": deleted.deleted,
        }),
        before.garbage_ratio(),
    ))
}

fn wait_for_auto_compact(
    state: &AppState,
    timeout_ms: u64,
) -> Result<(Option<Value>, f64), String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(500));
    let mut compacted = state
        .last_pack_compact()
        .map_err(|error| error.to_string())?;
    while compacted.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        compacted = state
            .last_pack_compact()
            .map_err(|error| error.to_string())?;
    }
    state.tasks.wait_idle(Duration::from_secs(20));
    let after = state.pack_stats().map_err(|error| error.to_string())?;
    Ok((
        compacted.map(|mark| serde_json::to_value(mark).unwrap_or(Value::Null)),
        after.garbage_ratio(),
    ))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WaitRequest {
    task_id: Option<String>,
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskIdRequest {
    task_id: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ThumbnailRequest {
    overwrite: bool,
    scope: TaskScope,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CheckRequest {
    mode: String,
    scope: TaskScope,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportRequest {
    scope: TaskScope,
    target_dir: String,
    #[serde(default)]
    include_manifest: bool,
    #[serde(default)]
    conflict: ConflictPolicy,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrphanMoveRequest {
    report_task_id: String,
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveReportRequest {
    task_id: String,
    path: String,
    #[serde(default)]
    group: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ScenarioCheckRequest {
    mode: String,
    scope: TaskScope,
    move_orphans: bool,
    save_report_path: Option<String>,
    save_group: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueCancelRequest {
    paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ImportCancelRequest {
    paths: Vec<String>,
    target: String,
    cancel_after_ms: u64,
}

impl Default for ImportCancelRequest {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            target: "library".to_owned(),
            cancel_after_ms: 250,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct HoldRequest {
    hold_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderLockRequest {
    folder_id: i64,
    folder_path: String,
    new_name: String,
    #[serde(default = "default_hold_ms")]
    hold_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AiLockRequest {
    image_id: i64,
    #[serde(default = "default_hold_ms")]
    hold_ms: u64,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct IdleGcRequest {
    timeout_ms: u64,
}

fn default_hold_ms() -> u64 {
    300
}

//! MCP 写工具：批量评分/标签/描述、移动、回收站、集合维护。
//!
//! 批量写入共用同一条规则：selector → id 集合 → `dryRun` 看数量 → 带 `confirmedCount`
//! 正式执行。正式执行前重新解析一次，数量或状态变化就拒绝，不落库。
//!
//! 两处例外：移动是后台任务，重新解析后交给任务总线占用 write lane；集合重命名/删除
//! 没有 selector，只按 `expectedItems` 核对集合当前图片数。

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use rmcp::schemars;
use rusqlite::params_from_iter;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::McpMode,
        state::AppState,
        tasks::TaskStartResult,
    },
    library::{
        collections, db::LibraryDb, history, metadata, paths::normalize_folder, placeholders, query,
    },
    mcp::{
        approval::ApprovalAction,
        selection::{self, Selector},
    },
};

use super::{ToolOutcome, read::compact_row};

/// 询问档只拦“影响超过这个张数”的批量写入；回收站与撤销/重做另外始终要确认。
pub const APPROVAL_THRESHOLD: usize = 100;
const SAMPLE_SIZE: usize = 10;
const MAX_FAILURES_REPORTED: usize = 50;
const SQL_CHUNK: usize = 400;

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImagesUpdateInput {
    pub selector: Selector,
    #[schemars(description = "0-5 星；与标签、描述三选一")]
    #[serde(default)]
    pub score: Option<i64>,
    #[schemars(description = "要添加的标签名；与评分、描述三选一")]
    #[serde(default)]
    pub add_tags: Vec<String>,
    #[schemars(description = "要移除的标签名；与评分、描述三选一")]
    #[serde(default)]
    pub remove_tags: Vec<String>,
    #[schemars(description = "新描述；selector 必须只解析出 1 张图")]
    #[serde(default)]
    pub description: Option<String>,
    #[schemars(description = "dryRun 返回的 selected 数；正式写入必须一致")]
    #[serde(default)]
    pub confirmed_count: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImagesMoveInput {
    pub selector: Selector,
    #[schemars(description = "目标文件夹相对路径；空字符串表示根目录")]
    pub folder_path: String,
    #[serde(default)]
    pub confirmed_count: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImagesTrashInput {
    pub selector: Selector,
    #[serde(default)]
    pub confirmed_count: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionsCreateInput {
    pub name: String,
    #[schemars(description = "可选；给定时把解析出的图片一起加入新集合")]
    #[serde(default)]
    pub selector: Option<Selector>,
    #[serde(default)]
    pub confirmed_count: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CollectionModifyAction {
    Rename,
    Delete,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionsModifyInput {
    pub action: CollectionModifyAction,
    pub collection_id: i64,
    #[schemars(description = "action=rename 时必填")]
    #[serde(default)]
    pub name: Option<String>,
    #[schemars(description = "删除前核对集合当前图片数；不一致就拒绝")]
    #[serde(default)]
    pub expected_items: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CollectionMembershipAction {
    Add,
    Remove,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CollectionsMembershipInput {
    pub action: CollectionMembershipAction,
    pub collection_id: i64,
    pub selector: Selector,
    #[serde(default)]
    pub confirmed_count: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

struct Inspection {
    database: Arc<LibraryDb>,
    root: PathBuf,
    /// selector 解析出的 id 总数，含不存在/状态不符的。
    selected: usize,
    active: Vec<i64>,
    trashed: Vec<i64>,
    missing: Vec<i64>,
    sample: Vec<Value>,
}

enum UpdateKind {
    Score(i64),
    AddTags(Vec<String>),
    RemoveTags(Vec<String>),
    Description(String),
}

fn mcp_mode(state: &AppState) -> McpMode {
    state.settings.snapshot().mcp.mode
}

fn ensure_writable(state: &AppState) -> AppResult<McpMode> {
    let mode = mcp_mode(state);
    if mode == McpMode::ReadOnly {
        return Err(AppError::message(
            "当前是只读模式，MCP 写入已关闭。请在设置 → MCP 中切换到询问或完全档。",
        ));
    }
    Ok(mode)
}

fn needs_approval(mode: McpMode, count: usize, dangerous: bool) -> bool {
    mode == McpMode::Ask && (dangerous || count > APPROVAL_THRESHOLD)
}

fn inspect(state: &AppState, selector: &Selector, sample_active: bool) -> AppResult<Inspection> {
    let context = state.mcp_context()?;
    let resolved = selection::resolve_ids(selector, &context, &state.ai)?;
    let selected = resolved.ids.len() + resolved.missing_ids.len();
    let states = image_states(&context.database, &resolved.ids)?;
    let mut active = Vec::new();
    let mut trashed = Vec::new();
    let mut missing = resolved.missing_ids;
    for id in resolved.ids {
        match states.get(&id) {
            Some(true) => trashed.push(id),
            Some(false) => active.push(id),
            None => missing.push(id),
        }
    }
    let sample_source = if sample_active { &active } else { &trashed };
    let sample = query::rows_by_ids(
        &context.database,
        &context.root,
        &sample_source[..sample_source.len().min(SAMPLE_SIZE)],
    )?
    .iter()
    .map(compact_row)
    .collect();
    Ok(Inspection {
        database: context.database,
        root: context.root,
        selected,
        active,
        trashed,
        missing,
        sample,
    })
}

fn image_states(database: &LibraryDb, ids: &[i64]) -> AppResult<HashMap<i64, bool>> {
    let mut states = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(SQL_CHUNK) {
        let marks = placeholders(chunk.len());
        database.with_reader(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT id, deleted_at IS NOT NULL FROM images
                 WHERE id IN ({marks}) AND purge_operation_id IS NULL"
            ))?;
            let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? != 0))
            })?;
            for row in rows {
                let (id, trashed) = row?;
                states.insert(id, trashed);
            }
            Ok(())
        })?;
    }
    Ok(states)
}

fn ineligible_failures(inspection: &Inspection, want_active: bool) -> Vec<Value> {
    let mut failures = inspection
        .missing
        .iter()
        .map(|id| json!({ "id": id, "reason": "不存在或正在释放" }))
        .collect::<Vec<_>>();
    if want_active {
        failures.extend(
            inspection
                .trashed
                .iter()
                .map(|id| json!({ "id": id, "reason": "已在回收站" })),
        );
    } else {
        failures.extend(
            inspection
                .active
                .iter()
                .map(|id| json!({ "id": id, "reason": "不在回收站" })),
        );
    }
    failures.truncate(MAX_FAILURES_REPORTED);
    failures
}

fn ineligible_outcome(inspection: &Inspection, want_active: bool) -> Option<ToolOutcome> {
    let failures = ineligible_failures(inspection, want_active);
    if failures.is_empty() {
        return None;
    }
    Some(ToolOutcome::value(json!({
        "status": "rejected",
        "reason": "部分图片不可用，未做任何修改",
        "selected": inspection.selected,
        "latestCount": inspection.selected,
        "failures": failures,
    })))
}

fn confirmed_outcome(explicit: Option<u64>, actual: usize, preview: Value) -> Option<ToolOutcome> {
    match explicit {
        Some(value) if value == actual as u64 => None,
        Some(_) => Some(ToolOutcome::value(json!({
            "status": "rejected",
            "reason": "数量与 confirmedCount 不一致，未做任何修改；请确认最新数量后重试",
            "latestCount": actual,
            "latest": preview,
        }))),
        None => Some(ToolOutcome::value(json!({
            "status": "rejected",
            "reason": "正式写入必须带 confirmedCount；请先 dryRun 查看 selected 数量",
            "latestCount": actual,
            "latest": preview,
        }))),
    }
}

fn sample_preview(inspection: &Inspection, will_change: u64, want_active: bool) -> Value {
    let skipped = if want_active {
        inspection.missing.len() + inspection.trashed.len()
    } else {
        inspection.missing.len() + inspection.active.len()
    };
    json!({
        "selected": inspection.selected,
        "willChange": will_change,
        "skipped": skipped as u64,
        "sample": inspection.sample,
    })
}

/// selector 解析为空：不写库，也不产生操作记录。
fn noop_outcome() -> ToolOutcome {
    ToolOutcome::value(json!({
        "status": "noop",
        "selected": 0,
        "affected": 0,
    }))
}

fn guide_preview(
    operation: &str,
    inspection: &Inspection,
    will_change: u64,
    consequences: &[&str],
    target: Option<&str>,
) -> Value {
    json!({
        "operation": operation,
        "target": target,
        "count": inspection.selected,
        "willChange": will_change,
        "sample": inspection.sample,
        "consequences": consequences,
    })
}

fn enqueue_approval<T: Serialize>(
    state: &AppState,
    tool: &str,
    title: String,
    input: &T,
    preview: Value,
) -> AppResult<ToolOutcome> {
    let arguments = serde_json::to_value(input)?;
    let approval = state.mcp_approvals().enqueue(
        title,
        ApprovalAction {
            tool: tool.to_owned(),
            arguments,
        },
        preview.clone(),
    );
    Ok(ToolOutcome::value(json!({
        "status": "pendingApproval",
        "approvalId": approval.id,
        "preview": preview,
        "message": "已放入应用内待确认队列；用 tasks_get(approvalId) 等待用户批准或拒绝。",
    })))
}

struct WriteFinish<'a> {
    tool: &'a str,
    operation_id: Option<i64>,
    selected: usize,
    affected: u64,
    close_viewer: bool,
    invalidate_similarity: bool,
    extra: Value,
}

fn finish_write(state: &AppState, finish: WriteFinish<'_>) -> Value {
    if let Some(operation_id) = finish.operation_id
        && let Some((database, _)) = state.current_library()
    {
        let _ = history::prefix_label(&database, operation_id, "AI · ");
    }
    if finish.operation_id.is_some() {
        state.after_mcp_write(
            json!({
                "source": "mcp",
                "tool": finish.tool,
                "operationId": finish.operation_id,
                "closeViewer": finish.close_viewer,
            }),
            finish.invalidate_similarity,
        );
    }
    let mut value = json!({
        "status": if finish.affected > 0 { "ok" } else { "noop" },
        "tool": finish.tool,
        "selected": finish.selected,
        "affected": finish.affected,
        "operationId": finish.operation_id,
    });
    if let Value::Object(extra) = finish.extra {
        for (key, extra_value) in extra {
            value_object_insert(&mut value, key, extra_value);
        }
    }
    value
}

fn value_object_insert(target: &mut Value, key: String, value: Value) {
    if let Value::Object(object) = target {
        object.insert(key, value);
    }
}

fn classify(input: &ImagesUpdateInput) -> AppResult<UpdateKind> {
    let present = input.score.is_some() as u8
        + (!input.add_tags.is_empty()) as u8
        + (!input.remove_tags.is_empty()) as u8
        + input.description.is_some() as u8;
    if present != 1 {
        return Err(AppError::message(
            "images_update 一次只能做一类修改：评分、加标签、删标签、单图描述四选一",
        ));
    }
    if let Some(score) = input.score {
        return Ok(UpdateKind::Score(score));
    }
    if !input.add_tags.is_empty() {
        return Ok(UpdateKind::AddTags(input.add_tags.clone()));
    }
    if !input.remove_tags.is_empty() {
        return Ok(UpdateKind::RemoveTags(input.remove_tags.clone()));
    }
    Ok(UpdateKind::Description(
        input.description.clone().unwrap_or_default(),
    ))
}

fn update_will_change(kind: &UpdateKind, inspection: &Inspection) -> u64 {
    match kind {
        UpdateKind::Description(text) if inspection.active.len() == 1 => {
            metadata::image_details(&inspection.database, &inspection.root, inspection.active[0])
                .map(|details| {
                    u64::from(
                        details.description.trim() != text.trim()
                            || details.description_source != "user"
                            || !details.description_model.is_empty(),
                    )
                })
                .unwrap_or(0)
        }
        // 标签可能一次改多条关系；这里报“会涉及的图片数”，实际 affected 由领域层返回。
        _ => inspection.active.len() as u64,
    }
}

fn conflict_error(active: &[crate::app::tasks::TaskSummary]) -> AppError {
    let label = active
        .first()
        .map(|task| task.label.as_str())
        .unwrap_or("其他任务");
    AppError::message(format!("「{label}」正在进行，请等待完成或取消"))
}

fn started_outcome(
    started: TaskStartResult,
    selected: usize,
    extra: Value,
) -> AppResult<ToolOutcome> {
    match started {
        TaskStartResult::Started { task } => {
            let mut value = json!({
                "status": "started",
                "taskId": task.task_id,
                "selected": selected,
            });
            if let Value::Object(extra) = extra {
                for (key, extra_value) in extra {
                    value_object_insert(&mut value, key, extra_value);
                }
            }
            Ok(ToolOutcome::task(task.task_id.clone(), value))
        }
        TaskStartResult::Conflict { active } => Err(conflict_error(&active)),
    }
}

pub fn images_update(state: &AppState, input: &ImagesUpdateInput) -> AppResult<ToolOutcome> {
    run_update(state, input, false)
}

pub fn execute_update(state: &AppState, input: &ImagesUpdateInput) -> AppResult<ToolOutcome> {
    run_update(state, input, true)
}

fn run_update(
    state: &AppState,
    input: &ImagesUpdateInput,
    approved: bool,
) -> AppResult<ToolOutcome> {
    let mode = ensure_writable(state)?;
    let kind = classify(input)?;
    let _exclusive = state.lock_mcp_writes();

    if input.dry_run {
        let inspection = inspect(state, &input.selector, true)?;
        if matches!(kind, UpdateKind::Description(_)) && inspection.selected != 1 {
            return Err(AppError::message(format!(
                "描述只允许 selector 解析为 1 张图片，当前是 {} 张",
                inspection.selected
            )));
        }
        let will_change = update_will_change(&kind, &inspection);
        let operation = match &kind {
            UpdateKind::Score(score) => format!("把评分改为 {score} 星"),
            UpdateKind::AddTags(_) => "添加标签".to_owned(),
            UpdateKind::RemoveTags(_) => "移除标签".to_owned(),
            UpdateKind::Description(_) => "修改描述".to_owned(),
        };
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "operation": operation,
            "selected": inspection.selected,
            "willChange": will_change,
            "skipped": (inspection.missing.len() + inspection.trashed.len()) as u64,
            "sample": inspection.sample,
        })));
    }

    let before = inspect(state, &input.selector, true)?;
    if matches!(kind, UpdateKind::Description(_)) && before.selected != 1 {
        return Err(AppError::message(format!(
            "描述只允许 selector 解析为 1 张图片，当前是 {} 张",
            before.selected
        )));
    }
    let before_will_change = update_will_change(&kind, &before);
    let preview = sample_preview(&before, before_will_change, true);
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, before.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&before, true) {
        return Ok(outcome);
    }
    if before.selected == 0 {
        return Ok(noop_outcome());
    }
    if needs_approval(mode, before.selected, false) && !approved {
        let operation = match &kind {
            UpdateKind::Score(score) => format!("给 {} 张图片评分 {score} 星", before.selected),
            UpdateKind::AddTags(tags) => {
                format!(
                    "给 {} 张图片添加标签「{}」",
                    before.selected,
                    tags.join("、")
                )
            }
            UpdateKind::RemoveTags(tags) => {
                format!(
                    "从 {} 张图片移除标签「{}」",
                    before.selected,
                    tags.join("、")
                )
            }
            UpdateKind::Description(_) => "修改 1 张图片的描述".to_owned(),
        };
        return enqueue_approval(
            state,
            "images_update",
            format!("AI · {operation}"),
            input,
            guide_preview(
                &operation,
                &before,
                before_will_change,
                &["评分、标签与描述都可以在操作记录里撤销"],
                None,
            ),
        );
    }

    // 正式写入：先占 write lane，再重新解析，避免解析与写入之间被别的写操作插队。
    let lane = state.acquire_write_lane()?;
    let inspection = inspect(state, &input.selector, true)?;
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, inspection.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&inspection, true) {
        return Ok(outcome);
    }
    let will_change = update_will_change(&kind, &inspection);
    let (affected, operation_id) = match kind {
        UpdateKind::Score(score) => {
            let (changed, operation_id) =
                metadata::set_scores_with_id(&inspection.database, &inspection.active, score)?;
            (changed, operation_id)
        }
        UpdateKind::AddTags(tags) => {
            let (changed, operation_id) =
                metadata::add_tags_with_id(&inspection.database, &inspection.active, &tags)?;
            (changed, operation_id)
        }
        UpdateKind::RemoveTags(tags) => {
            let (changed, operation_id) =
                metadata::remove_tags_with_id(&inspection.database, &inspection.active, &tags)?;
            (changed, operation_id)
        }
        UpdateKind::Description(text) => {
            metadata::update_description_with_id(&inspection.database, inspection.active[0], &text)?
        }
    };
    drop(lane);
    Ok(ToolOutcome::value(finish_write(
        state,
        WriteFinish {
            tool: "images_update",
            operation_id,
            selected: inspection.selected,
            affected,
            close_viewer: false,
            invalidate_similarity: false,
            extra: json!({ "willChange": will_change }),
        },
    )))
}

pub fn images_move(state: &AppState, input: &ImagesMoveInput) -> AppResult<ToolOutcome> {
    run_move(state, input, false)
}

pub fn execute_move(state: &AppState, input: &ImagesMoveInput) -> AppResult<ToolOutcome> {
    run_move(state, input, true)
}

fn run_move(state: &AppState, input: &ImagesMoveInput, approved: bool) -> AppResult<ToolOutcome> {
    let mode = ensure_writable(state)?;
    let folder_path = normalize_folder(&input.folder_path)?;
    let target = if folder_path.is_empty() {
        "根目录".to_owned()
    } else {
        format!("「{folder_path}」")
    };
    let _exclusive = state.lock_mcp_writes();

    let before = inspect(state, &input.selector, true)?;
    let preview = sample_preview(&before, before.active.len() as u64, true);
    if input.dry_run {
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "operation": format!("移动到{target}"),
            "selected": before.selected,
            "willChange": before.active.len(),
            "skipped": (before.missing.len() + before.trashed.len()) as u64,
            "sample": before.sample,
        })));
    }
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, before.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&before, true) {
        return Ok(outcome);
    }
    if before.selected == 0 {
        return Ok(noop_outcome());
    }
    if needs_approval(mode, before.selected, false) && !approved {
        return enqueue_approval(
            state,
            "images_move",
            format!("AI · 把 {} 张图片移动到{target}", before.selected),
            input,
            guide_preview(
                &format!("移动到{target}"),
                &before,
                before.active.len() as u64,
                &["原图会从旧文件夹移动到目标文件夹", "可以撤销"],
                Some(&target),
            ),
        );
    }

    // 任务是后台的 write lane 占用者；这里先确认当前没有别的任务，避免被静默排队。
    if state.tasks.any_busy() {
        return Err(AppError::message(
            "有任务正在进行，请等待完成或取消后再移动",
        ));
    }
    let inspection = inspect(state, &input.selector, true)?;
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, inspection.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&inspection, true) {
        return Ok(outcome);
    }
    let selected = inspection.selected;
    let started = state.start_mcp_move_task(inspection.active, folder_path)?;
    started_outcome(started, selected, json!({ "target": target }))
}

pub fn images_trash(state: &AppState, input: &ImagesTrashInput) -> AppResult<ToolOutcome> {
    run_trash(state, input, false)
}

pub fn execute_trash(state: &AppState, input: &ImagesTrashInput) -> AppResult<ToolOutcome> {
    run_trash(state, input, true)
}

fn run_trash(state: &AppState, input: &ImagesTrashInput, approved: bool) -> AppResult<ToolOutcome> {
    let mode = ensure_writable(state)?;
    let _exclusive = state.lock_mcp_writes();
    let before = inspect(state, &input.selector, true)?;
    let preview = sample_preview(&before, before.active.len() as u64, true);
    if input.dry_run {
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "operation": "移入回收站",
            "selected": before.selected,
            "willChange": before.active.len(),
            "skipped": (before.missing.len() + before.trashed.len()) as u64,
            "sample": before.sample,
        })));
    }
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, before.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&before, true) {
        return Ok(outcome);
    }
    if before.selected == 0 {
        return Ok(noop_outcome());
    }
    // 移入回收站不论张数，询问档都要确认。
    if mode == McpMode::Ask && !approved {
        return enqueue_approval(
            state,
            "images_trash",
            format!("AI · 把 {} 张图片移入回收站", before.selected),
            input,
            guide_preview(
                "移入回收站",
                &before,
                before.active.len() as u64,
                &[
                    "图片会离开浏览与搜索结果",
                    "可以从回收站恢复",
                    "不会删除原图",
                ],
                None,
            ),
        );
    }

    let lane = state.acquire_write_lane()?;
    let inspection = inspect(state, &input.selector, true)?;
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, inspection.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&inspection, true) {
        return Ok(outcome);
    }
    let (affected, operation_id) =
        metadata::set_trashed_with_id(&inspection.database, &inspection.active, true)?;
    drop(lane);
    Ok(ToolOutcome::value(finish_write(
        state,
        WriteFinish {
            tool: "images_trash",
            operation_id,
            selected: inspection.selected,
            affected,
            close_viewer: true,
            invalidate_similarity: true,
            extra: Value::Null,
        },
    )))
}

pub fn images_restore(state: &AppState, input: &ImagesTrashInput) -> AppResult<ToolOutcome> {
    run_restore(state, input, false)
}

pub fn execute_restore(state: &AppState, input: &ImagesTrashInput) -> AppResult<ToolOutcome> {
    run_restore(state, input, true)
}

fn run_restore(
    state: &AppState,
    input: &ImagesTrashInput,
    approved: bool,
) -> AppResult<ToolOutcome> {
    let mode = ensure_writable(state)?;
    let _exclusive = state.lock_mcp_writes();
    let before = inspect(state, &input.selector, false)?;
    let preview = sample_preview(&before, before.trashed.len() as u64, false);
    if input.dry_run {
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "operation": "恢复图片",
            "selected": before.selected,
            "willChange": before.trashed.len(),
            "skipped": (before.missing.len() + before.active.len()) as u64,
            "sample": before.sample,
        })));
    }
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, before.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&before, false) {
        return Ok(outcome);
    }
    if before.selected == 0 {
        return Ok(noop_outcome());
    }
    if needs_approval(mode, before.selected, false) && !approved {
        return enqueue_approval(
            state,
            "images_restore",
            format!("AI · 恢复 {} 张图片", before.selected),
            input,
            guide_preview(
                "恢复图片",
                &before,
                before.trashed.len() as u64,
                &["图片会回到原来的文件夹", "可以撤销"],
                None,
            ),
        );
    }

    let lane = state.acquire_write_lane()?;
    let inspection = inspect(state, &input.selector, false)?;
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, inspection.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&inspection, false) {
        return Ok(outcome);
    }
    let (affected, operation_id) =
        metadata::set_trashed_with_id(&inspection.database, &inspection.trashed, false)?;
    drop(lane);
    Ok(ToolOutcome::value(finish_write(
        state,
        WriteFinish {
            tool: "images_restore",
            operation_id,
            selected: inspection.selected,
            affected,
            close_viewer: false,
            invalidate_similarity: true,
            extra: Value::Null,
        },
    )))
}

pub fn collections_create(
    state: &AppState,
    input: &CollectionsCreateInput,
) -> AppResult<ToolOutcome> {
    run_collection_create(state, input, false)
}

pub fn execute_collection_create(
    state: &AppState,
    input: &CollectionsCreateInput,
) -> AppResult<ToolOutcome> {
    run_collection_create(state, input, true)
}

fn run_collection_create(
    state: &AppState,
    input: &CollectionsCreateInput,
    approved: bool,
) -> AppResult<ToolOutcome> {
    let mode = ensure_writable(state)?;
    let _exclusive = state.lock_mcp_writes();
    let inspection = match &input.selector {
        Some(selector) => Some(inspect(state, selector, true)?),
        None => None,
    };
    let selected = inspection.as_ref().map(|item| item.selected).unwrap_or(0);
    let active_count = inspection
        .as_ref()
        .map(|item| item.active.len())
        .unwrap_or(0);
    let sample = inspection
        .as_ref()
        .map(|item| item.sample.clone())
        .unwrap_or_default();
    let preview = json!({
        "selected": selected,
        "willChange": active_count,
        "skipped": inspection
            .as_ref()
            .map(|item| item.missing.len() + item.trashed.len())
            .unwrap_or(0),
        "sample": sample,
    });
    if input.dry_run {
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "operation": format!("创建集合「{}」", input.name.trim()),
            "selected": selected,
            "willChange": active_count,
            "sample": sample,
        })));
    }
    if input.selector.is_some()
        && let Some(outcome) = confirmed_outcome(input.confirmed_count, selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(inspection) = &inspection
        && let Some(outcome) = ineligible_outcome(inspection, true)
    {
        return Ok(outcome);
    }
    // 带 selector 却一张都没解析到：不建空集合，让 AI 知道这次没有命中。
    if input.selector.is_some() && selected == 0 {
        return Ok(noop_outcome());
    }
    if needs_approval(mode, selected, false) && !approved {
        let preview = match &inspection {
            Some(inspection) => guide_preview(
                "创建集合",
                inspection,
                active_count as u64,
                &["集合与成员关系只影响组织方式，不改动原图", "可以撤销"],
                None,
            ),
            None => preview.clone(),
        };
        return enqueue_approval(
            state,
            "collections_create",
            format!(
                "AI · 创建集合「{}」并加入 {selected} 张图片",
                input.name.trim()
            ),
            input,
            preview,
        );
    }

    let lane = state.acquire_write_lane()?;
    let inspection = match &input.selector {
        Some(selector) => Some(inspect(state, selector, true)?),
        None => None,
    };
    let selected = inspection.as_ref().map(|item| item.selected).unwrap_or(0);
    if input.selector.is_some()
        && let Some(outcome) = confirmed_outcome(input.confirmed_count, selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(inspection) = &inspection
        && let Some(outcome) = ineligible_outcome(inspection, true)
    {
        return Ok(outcome);
    }
    if input.selector.is_some() && selected == 0 {
        return Ok(noop_outcome());
    }
    let (database, _) = state
        .current_library()
        .ok_or_else(|| AppError::message("尚未打开图库"))?;
    let (collection, added, operation_id) = collections::create_with_images(
        &database,
        &input.name,
        inspection
            .as_ref()
            .map(|item| item.active.as_slice())
            .unwrap_or(&[]),
    )?;
    drop(lane);
    Ok(ToolOutcome::value(finish_write(
        state,
        WriteFinish {
            tool: "collections_create",
            operation_id,
            selected,
            // 集合本身也算一处改动；否则空集合会被误报成 noop。
            affected: 1 + added,
            close_viewer: false,
            invalidate_similarity: false,
            extra: json!({ "collection": collection, "added": added }),
        },
    )))
}

/// 重命名与删除集合不在询问档的拦截范围内，因此没有单独的 `execute_` 入口。
pub fn collections_modify(
    state: &AppState,
    input: &CollectionsModifyInput,
) -> AppResult<ToolOutcome> {
    ensure_writable(state)?;
    let _exclusive = state.lock_mcp_writes();
    let (database, _) = state
        .current_library()
        .ok_or_else(|| AppError::message("尚未打开图库"))?;
    let current = collections::list(&database)?
        .into_iter()
        .find(|collection| collection.id == input.collection_id)
        .ok_or_else(|| AppError::message("集合不存在"))?;
    if input.dry_run {
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "operation": match input.action {
                CollectionModifyAction::Rename => "重命名集合",
                CollectionModifyAction::Delete => "删除集合",
            },
            "collection": current,
        })));
    }
    if let Some(expected) = input.expected_items
        && expected != current.image_count
    {
        return Ok(ToolOutcome::value(json!({
            "status": "rejected",
            "reason": "集合当前图片数与 expectedItems 不一致，未做任何修改",
            "latestCount": current.image_count,
            "collection": current,
        })));
    }

    let lane = state.acquire_write_lane()?;
    let (affected, operation_id, extra) = match input.action {
        CollectionModifyAction::Rename => {
            let name = input
                .name
                .as_deref()
                .ok_or_else(|| AppError::message("action=rename 时必须提供 name"))?;
            let (_, operation_id) =
                collections::rename_with_id(&database, input.collection_id, name)?;
            (
                u64::from(operation_id.is_some()),
                operation_id,
                json!({ "name": name.trim() }),
            )
        }
        CollectionModifyAction::Delete => {
            let (affected, operation_id) =
                collections::delete_with_id(&database, input.collection_id)?;
            (affected, operation_id, json!({ "deleted": true }))
        }
    };
    drop(lane);
    Ok(ToolOutcome::value(finish_write(
        state,
        WriteFinish {
            tool: "collections_modify",
            operation_id,
            selected: 1,
            // 删除空集合时成员数为 0，但集合结构确实变了。
            affected: affected.max(1),
            close_viewer: false,
            invalidate_similarity: false,
            extra,
        },
    )))
}

pub fn collections_membership(
    state: &AppState,
    input: &CollectionsMembershipInput,
) -> AppResult<ToolOutcome> {
    run_collection_membership(state, input, false)
}

pub fn execute_collection_membership(
    state: &AppState,
    input: &CollectionsMembershipInput,
) -> AppResult<ToolOutcome> {
    run_collection_membership(state, input, true)
}

fn run_collection_membership(
    state: &AppState,
    input: &CollectionsMembershipInput,
    approved: bool,
) -> AppResult<ToolOutcome> {
    let mode = ensure_writable(state)?;
    let _exclusive = state.lock_mcp_writes();
    let before = inspect(state, &input.selector, true)?;
    let preview = sample_preview(&before, before.active.len() as u64, true);
    let operation = match input.action {
        CollectionMembershipAction::Add => "加入集合",
        CollectionMembershipAction::Remove => "移出集合",
    };
    if input.dry_run {
        return Ok(ToolOutcome::value(json!({
            "status": "dryRun",
            "operation": operation,
            "selected": before.selected,
            "willChange": before.active.len(),
            "skipped": (before.missing.len() + before.trashed.len()) as u64,
            "sample": before.sample,
        })));
    }
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, before.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&before, true) {
        return Ok(outcome);
    }
    if before.selected == 0 {
        return Ok(noop_outcome());
    }
    if needs_approval(mode, before.selected, false) && !approved {
        return enqueue_approval(
            state,
            "collections_membership",
            format!("AI · 把 {} 张图片{operation}", before.selected),
            input,
            guide_preview(
                operation,
                &before,
                before.active.len() as u64,
                &["只改集合成员，不改动原图", "可以撤销"],
                None,
            ),
        );
    }

    let lane = state.acquire_write_lane()?;
    let inspection = inspect(state, &input.selector, true)?;
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, inspection.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&inspection, true) {
        return Ok(outcome);
    }
    let (affected, operation_id) = match input.action {
        CollectionMembershipAction::Add => collections::add_images_with_id(
            &inspection.database,
            input.collection_id,
            &inspection.active,
        )?,
        CollectionMembershipAction::Remove => collections::remove_images_with_id(
            &inspection.database,
            input.collection_id,
            &inspection.active,
        )?,
    };
    drop(lane);
    Ok(ToolOutcome::value(finish_write(
        state,
        WriteFinish {
            tool: "collections_membership",
            operation_id,
            selected: inspection.selected,
            affected,
            close_viewer: false,
            invalidate_similarity: false,
            extra: json!({ "action": operation }),
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_requires_exactly_one_kind() {
        let base = ImagesUpdateInput {
            selector: Selector::Ids {
                ids: vec![1],
                exclude_ids: Vec::new(),
            },
            score: None,
            add_tags: Vec::new(),
            remove_tags: Vec::new(),
            description: None,
            confirmed_count: None,
            dry_run: false,
        };
        assert!(classify(&base).is_err());
        let mut score = base.clone();
        score.score = Some(4);
        assert!(matches!(classify(&score), Ok(UpdateKind::Score(4))));
        score.add_tags = vec!["人物".into()];
        assert!(classify(&score).is_err());
    }

    #[test]
    fn approval_threshold_and_modes() {
        assert!(!needs_approval(McpMode::Full, 500, true));
        assert!(!needs_approval(McpMode::ReadOnly, 500, true));
        assert!(needs_approval(McpMode::Ask, APPROVAL_THRESHOLD + 1, false));
        assert!(needs_approval(McpMode::Ask, 1, true));
        assert!(!needs_approval(McpMode::Ask, APPROVAL_THRESHOLD, false));
    }

    #[test]
    fn confirmed_count_must_be_exact_and_present() {
        assert!(confirmed_outcome(Some(37), 37, json!({})).is_none());

        let missing = confirmed_outcome(None, 37, json!({ "selected": 37 }))
            .unwrap()
            .value;
        assert_eq!(missing["status"], "rejected");
        assert_eq!(missing["latestCount"], 37);

        let mismatch = confirmed_outcome(Some(36), 37, json!({ "selected": 37 }))
            .unwrap()
            .value;
        assert_eq!(mismatch["status"], "rejected");
        assert_eq!(mismatch["latestCount"], 37);
        assert_eq!(mismatch["latest"]["selected"], 37);
    }
}

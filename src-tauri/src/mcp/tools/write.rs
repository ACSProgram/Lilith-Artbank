//! MCP 写工具：批量评分/标签/描述、移动、回收站、集合维护。
//!
//! 批量写入共用同一条规则：selector → id 集合 → `dryRun` 看数量 → 带 `confirmedCount`
//! 正式执行。正式执行前重新解析一次，数量或状态变化就拒绝，不落库。
//!
//! 两处例外：移动是后台任务，重新解析后交给任务总线申请写资源；集合重命名/删除
//! 没有 selector，只按 `expectedItems` 核对集合当前图片数。

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use rmcp::schemars;
use rusqlite::{OptionalExtension, params_from_iter};
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

use super::{ToolOutcome, merge_object, read::compact_row};

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

pub(super) fn ensure_writable(state: &AppState) -> AppResult<McpMode> {
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
    let temp_path = context.database.with_reader(crate::library::temp::path)?;
    let sample = query::rows_by_ids(
        &context.database,
        &context.root,
        &sample_source[..sample_source.len().min(SAMPLE_SIZE)],
    )?
    .iter()
    .map(|row| compact_row(row, temp_path.as_deref()))
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

/// 确认单与 dryRun 共用的一份数量说明：`skipped` 与 `willChange` 加起来一定等于 `selected`，
/// 其中 skipped 既包含不可用的图片，也包含“已经是目标状态、不需要改”的图片。
fn sample_preview(inspection: &Inspection, will_change: u64) -> Value {
    json!({
        "selected": inspection.selected,
        "willChange": will_change,
        "skipped": (inspection.selected as u64).saturating_sub(will_change),
        "sample": inspection.sample,
    })
}

/// 目标状态与现状一致：不写库，也不产生操作记录。
fn no_change_outcome(selected: usize) -> ToolOutcome {
    ToolOutcome::value(json!({
        "status": "noop",
        "selected": selected,
        "affected": 0,
        "reason": "这些图片已经是目标状态，没有需要改动的内容",
    }))
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

pub(super) fn enqueue_approval<T: Serialize>(
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

pub(super) struct WriteFinish<'a> {
    pub tool: &'a str,
    pub operation_id: Option<i64>,
    pub selected: usize,
    pub affected: u64,
    pub close_viewer: bool,
    pub invalidate_similarity: bool,
    /// 前端据此判断列表、选择与详情是否需要失效；拿不到受影响 id。
    pub change: Value,
    pub extra: Value,
}

pub(super) fn finish_write(state: &AppState, finish: WriteFinish<'_>) -> Value {
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
                "change": finish.change,
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
    merge_object(&mut value, finish.extra);
    value
}

fn update_change(kind: &UpdateKind) -> Value {
    match kind {
        UpdateKind::Score(score) => {
            json!({ "kind": "metadata", "fields": ["score"], "score": score })
        }
        UpdateKind::AddTags(_) => {
            json!({ "kind": "metadata", "fields": ["tags"], "tagAction": "add" })
        }
        UpdateKind::RemoveTags(_) => {
            json!({ "kind": "metadata", "fields": ["tags"], "tagAction": "remove" })
        }
        UpdateKind::Description(text) => {
            // 只回传是否为空：前端据此判断“有/无描述”筛选下的成员变化，不必搬运整段文本。
            json!({ "kind": "metadata", "fields": ["description"], "descriptionEmpty": text.trim().is_empty() })
        }
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

/// 实际会改动的图片数：已经是目标状态的图片不计入，避免 dryRun 报大数。
fn update_will_change(kind: &UpdateKind, inspection: &Inspection) -> AppResult<u64> {
    match kind {
        UpdateKind::Description(text) if inspection.active.len() == 1 => Ok(
            metadata::image_details(&inspection.database, &inspection.root, inspection.active[0])
                .map(|details| {
                    u64::from(
                        details.description.trim() != text.trim()
                            || details.description_source != "user"
                            || !details.description_model.is_empty(),
                    )
                })
                .unwrap_or(0),
        ),
        UpdateKind::Description(_) => Ok(0),
        UpdateKind::Score(score) => {
            count_score_changes(&inspection.database, &inspection.active, *score)
        }
        UpdateKind::AddTags(names) => {
            count_tag_changes(&inspection.database, &inspection.active, names, true)
        }
        UpdateKind::RemoveTags(names) => {
            count_tag_changes(&inspection.database, &inspection.active, names, false)
        }
    }
}

fn count_score_changes(database: &LibraryDb, ids: &[i64], score: i64) -> AppResult<u64> {
    let mut changed = 0_u64;
    for chunk in ids.chunks(SQL_CHUNK) {
        let marks = placeholders(chunk.len());
        let mut values: Vec<i64> = chunk.to_vec();
        values.push(score);
        changed += database.with_reader(|connection| {
            let count: i64 = connection.query_row(
                &format!("SELECT count(*) FROM images WHERE id IN ({marks}) AND score <> ?"),
                params_from_iter(values.iter()),
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })?;
    }
    Ok(changed)
}

/// 会新增/移除标签关系的图片数。
///
/// 要加的标签还不存在时，标签行会先建出来，所以每张图都算一次改动；
/// 要删的标签不存在时，一条关系也不会动。
fn count_tag_changes(
    database: &LibraryDb,
    ids: &[i64],
    names: &[String],
    adding: bool,
) -> AppResult<u64> {
    let names = normalize_tag_names(names);
    if ids.is_empty() || names.is_empty() {
        return Ok(0);
    }
    let Some(tag_ids) = tag_ids_by_name(database, &names)? else {
        return Ok(if adding { ids.len() as u64 } else { 0 });
    };
    let condition = if adding { "NOT EXISTS" } else { "EXISTS" };
    let tag_marks = placeholders(tag_ids.len());
    let mut changed = 0_u64;
    for chunk in ids.chunks(SQL_CHUNK) {
        let id_marks = placeholders(chunk.len());
        let mut values: Vec<i64> = chunk.to_vec();
        values.extend(tag_ids.iter().copied());
        changed += database.with_reader(|connection| {
            let count: i64 = connection.query_row(
                &format!(
                    "SELECT count(*) FROM images
                     WHERE id IN ({id_marks}) AND {condition} (
                        SELECT 1 FROM image_tags
                        WHERE image_tags.image_id = images.id
                          AND image_tags.tag_id IN ({tag_marks})
                     )"
                ),
                params_from_iter(values.iter()),
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })?;
    }
    Ok(changed)
}

fn normalize_tag_names(names: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for name in names {
        let name = name.trim();
        if !name.is_empty() && seen.insert(name.to_lowercase()) {
            result.push(name.to_owned());
        }
    }
    result
}

/// 全部标签都存在时返回 id；有一个不存在就返回 `None`。
fn tag_ids_by_name(database: &LibraryDb, names: &[String]) -> AppResult<Option<Vec<i64>>> {
    database.with_reader(|connection| {
        let mut statement =
            connection.prepare("SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE")?;
        let mut ids = Vec::with_capacity(names.len());
        for name in names {
            let id: Option<i64> = statement
                .query_row([name], |row| row.get(0))
                .optional()
                .map_err(AppError::from)?;
            match id {
                Some(id) => ids.push(id),
                None => return Ok(None),
            }
        }
        Ok(Some(ids))
    })
}

/// 已经在这个文件夹里的图片数：它们不会移动，dryRun 里应当算跳过。
///
/// 用 `folders.path = ?` 的精确比较，与移动任务内部 `old_folder == folder_path` 的判断同口径。
fn count_already_in_folder(database: &LibraryDb, ids: &[i64], folder_path: &str) -> AppResult<u64> {
    let mut count = 0_u64;
    for chunk in ids.chunks(SQL_CHUNK) {
        let marks = placeholders(chunk.len());
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 1);
        params.extend(chunk.iter().map(|id| rusqlite::types::Value::from(*id)));
        params.push(rusqlite::types::Value::from(folder_path.to_owned()));
        count += database.with_reader(|connection| {
            let matched: i64 = connection.query_row(
                &format!(
                    "SELECT count(*) FROM images JOIN folders ON folders.id = images.folder_id
                     WHERE images.id IN ({marks}) AND folders.path = ?"
                ),
                params_from_iter(params.iter()),
                |row| row.get(0),
            )?;
            Ok(matched as u64)
        })?;
    }
    Ok(count)
}

/// 集合里实际会新增/移除的图片数；已经是目标状态的图片不计入。
fn count_membership_changes(
    database: &LibraryDb,
    collection_id: i64,
    ids: &[i64],
    adding: bool,
) -> AppResult<u64> {
    let condition = if adding { "NOT EXISTS" } else { "EXISTS" };
    let mut changed = 0_u64;
    for chunk in ids.chunks(SQL_CHUNK) {
        let marks = placeholders(chunk.len());
        // 占位符按出现顺序编号：先 IN 里的 id，再子查询的 collection_id。
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 1);
        params.extend(chunk.iter().map(|id| rusqlite::types::Value::from(*id)));
        params.push(rusqlite::types::Value::from(collection_id));
        changed += database.with_reader(|connection| {
            let count: i64 = connection.query_row(
                &format!(
                    "SELECT count(*) FROM images
                     WHERE id IN ({marks}) AND {condition} (
                        SELECT 1 FROM collection_items
                        WHERE collection_items.collection_id = ?
                          AND collection_items.image_id = images.id
                     )"
                ),
                params_from_iter(params.iter()),
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })?;
    }
    Ok(changed)
}

/// 集合不存在时早点报错，不要让确认卡指向一个不存在的集合。
fn ensure_collection_exists(database: &LibraryDb, collection_id: i64) -> AppResult<()> {
    let exists: bool = database.with_reader(|connection| {
        Ok(connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM collections WHERE id = ?1)",
            [collection_id],
            |row| row.get(0),
        )?)
    })?;
    if !exists {
        return Err(AppError::message("集合不存在"));
    }
    Ok(())
}

/// 选择里有临时图片时整批拒绝：它们不能移动、移入回收站或加入集合，
/// 同步拦下比先让用户确认一次再失败好。
fn temporary_outcome(inspection: &Inspection) -> AppResult<Option<ToolOutcome>> {
    let temporary = crate::library::temp::ids_in(&inspection.database, &inspection.active)?;
    if temporary.is_empty() {
        return Ok(None);
    }
    let mut ids: Vec<i64> = temporary.into_iter().collect();
    ids.sort_unstable();
    ids.truncate(MAX_FAILURES_REPORTED);
    Ok(Some(ToolOutcome::value(json!({
        "status": "rejected",
        "reason": "选择里有临时区的图片；临时图片不能移动、移入回收站或加入集合，关闭或切换图库时会自动清空",
        "selected": inspection.selected,
        "latestCount": inspection.selected,
        "temporaryIds": ids,
    }))))
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
            merge_object(&mut value, extra);
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
        let will_change = update_will_change(&kind, &inspection)?;
        let operation = match &kind {
            UpdateKind::Score(score) => format!("把评分改为 {score} 星"),
            UpdateKind::AddTags(_) => "添加标签".to_owned(),
            UpdateKind::RemoveTags(_) => "移除标签".to_owned(),
            UpdateKind::Description(_) => "修改描述".to_owned(),
        };
        let mut value = json!({ "status": "dryRun", "operation": operation });
        merge_object(&mut value, sample_preview(&inspection, will_change));
        return Ok(ToolOutcome::value(value));
    }

    let before = inspect(state, &input.selector, true)?;
    if matches!(kind, UpdateKind::Description(_)) && before.selected != 1 {
        return Err(AppError::message(format!(
            "描述只允许 selector 解析为 1 张图片，当前是 {} 张",
            before.selected
        )));
    }
    let before_will_change = update_will_change(&kind, &before)?;
    let preview = sample_preview(&before, before_will_change);
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
    // 目标状态与现状一致：不写库，也不留一条 affected=0 的空记录。
    if before_will_change == 0 {
        return Ok(no_change_outcome(before.selected));
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

    // 正式写入：先占元数据写，再重新解析，避免解析与写入之间被别的写操作插队。
    let lane = state.begin_metadata_write("修改图片信息")?;
    let inspection = inspect(state, &input.selector, true)?;
    if let Some(outcome) =
        confirmed_outcome(input.confirmed_count, inspection.selected, preview.clone())
    {
        return Ok(outcome);
    }
    if let Some(outcome) = ineligible_outcome(&inspection, true) {
        return Ok(outcome);
    }
    let will_change = update_will_change(&kind, &inspection)?;
    if will_change == 0 {
        drop(lane);
        return Ok(no_change_outcome(inspection.selected));
    }
    let change = update_change(&kind);
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
            change,
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
    // 临时图片本来就不能移动：同步拒绝，不要先问用户一次再失败。
    if let Some(outcome) = temporary_outcome(&before)? {
        return Ok(outcome);
    }
    let already = count_already_in_folder(&before.database, &before.active, &folder_path)?;
    let will_change = before.active.len() as u64 - already;
    let preview = sample_preview(&before, will_change);
    if input.dry_run {
        let mut value = json!({
            "status": "dryRun",
            "operation": format!("移动到{target}"),
            "alreadyInTarget": already,
        });
        merge_object(&mut value, preview.clone());
        return Ok(ToolOutcome::value(value));
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
    if will_change == 0 {
        return Ok(no_change_outcome(before.selected));
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
                will_change,
                &["原图会从旧文件夹移动到目标文件夹", "可以撤销"],
                Some(&target),
            ),
        );
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
    // 临时图片不能移入回收站：同步拒绝，别弹一张注定失败的确认卡。
    if let Some(outcome) = temporary_outcome(&before)? {
        return Ok(outcome);
    }
    let preview = sample_preview(&before, before.active.len() as u64);
    if input.dry_run {
        let mut value = json!({ "status": "dryRun", "operation": "移入回收站" });
        merge_object(&mut value, preview.clone());
        return Ok(ToolOutcome::value(value));
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

    let lane = state.begin_metadata_write("修改图片信息")?;
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
            change: json!({ "kind": "membership" }),
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
    let preview = sample_preview(&before, before.trashed.len() as u64);
    if input.dry_run {
        let mut value = json!({ "status": "dryRun", "operation": "恢复图片" });
        merge_object(&mut value, preview.clone());
        return Ok(ToolOutcome::value(value));
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

    let lane = state.begin_metadata_write("修改图片信息")?;
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
            change: json!({ "kind": "membership" }),
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
    // 临时图片不能加入集合；有就整批拒绝，别让用户确认一次再失败。
    if let Some(inspection) = &inspection
        && let Some(outcome) = temporary_outcome(inspection)?
    {
        return Ok(outcome);
    }
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
        let mut value = json!({
            "status": "dryRun",
            "operation": format!("创建集合「{}」", input.name.trim()),
        });
        merge_object(&mut value, preview.clone());
        return Ok(ToolOutcome::value(value));
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

    let lane = state.begin_metadata_write("修改图片信息")?;
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
            change: json!({ "kind": "collection" }),
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

    let lane = state.begin_metadata_write("修改图片信息")?;
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
            change: json!({ "kind": "collection" }),
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
    ensure_collection_exists(
        &state
            .current_library()
            .ok_or_else(|| AppError::message("尚未打开图库"))?
            .0,
        input.collection_id,
    )?;
    let before = inspect(state, &input.selector, true)?;
    if let Some(outcome) = temporary_outcome(&before)? {
        return Ok(outcome);
    }
    let adding = matches!(input.action, CollectionMembershipAction::Add);
    let will_change = count_membership_changes(
        &before.database,
        input.collection_id,
        &before.active,
        adding,
    )?;
    let preview = sample_preview(&before, will_change);
    let operation = match input.action {
        CollectionMembershipAction::Add => "加入集合",
        CollectionMembershipAction::Remove => "移出集合",
    };
    if input.dry_run {
        let mut value = json!({ "status": "dryRun", "operation": operation });
        merge_object(&mut value, preview.clone());
        return Ok(ToolOutcome::value(value));
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
    if will_change == 0 {
        return Ok(no_change_outcome(before.selected));
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
                will_change,
                &["只改集合成员，不改动原图", "可以撤销"],
                None,
            ),
        );
    }

    let lane = state.begin_metadata_write("修改图片信息")?;
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
            change: json!({ "kind": "collection" }),
            extra: json!({ "action": operation }),
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 预检 SQL 只靠库表，不需要完整的 AppState；用隔离的临时库验证计数口径。
    fn fixture() -> (tempfile::TempDir, Arc<LibraryDb>) {
        use rusqlite::params;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = crate::library::service::create(&root, "写工具测试", 1).unwrap();
        session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO folders(path, created_at) VALUES ('作品', 1)",
                    [],
                )?;
                let folder = connection.last_insert_rowid();
                for (id, score) in [(1_i64, 5_i64), (2, 3), (3, 1)] {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', ?3, 10, 10, 10, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), ?4, '', 1, 1)",
                        params![id, format!("{id:0>64}"), folder, score],
                    )?;
                }
                connection.execute("INSERT INTO tags(name) VALUES ('人物')", [])?;
                let people = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO image_tags(image_id, tag_id) VALUES (1, ?1)",
                    [people],
                )?;
                connection.execute(
                    "INSERT INTO collections(name, sort_order, created_at, updated_at)
                     VALUES ('精选', 0, 1, 1)",
                    [],
                )?;
                let collection = connection.last_insert_rowid();
                connection.execute(
                    "INSERT INTO collection_items(collection_id, image_id, position)
                     VALUES (?1, 1, 0)",
                    [collection],
                )?;
                Ok(())
            })
            .unwrap();
        (temp, session.db)
    }

    #[test]
    fn counts_only_rows_that_really_change() {
        let (_temp, database) = fixture();
        let ids = [1_i64, 2, 3];

        // 评分已经是 5 星的图片不算改动。
        assert_eq!(count_score_changes(&database, &ids, 5).unwrap(), 2);
        assert_eq!(count_score_changes(&database, &ids, 3).unwrap(), 2);

        // 加标签：已经有「人物」的图片不算；标签不存在时每张图都要建关系。
        assert_eq!(
            count_tag_changes(&database, &ids, &["人物".into()], true).unwrap(),
            2
        );
        assert_eq!(
            count_tag_changes(&database, &ids, &["新标签".into()], true).unwrap(),
            3
        );
        // 删标签：没有这个标签的图片不算；标签不存在时一条关系也不用动。
        assert_eq!(
            count_tag_changes(&database, &ids, &["人物".into()], false).unwrap(),
            1
        );
        assert_eq!(
            count_tag_changes(&database, &ids, &["不存在".into()], false).unwrap(),
            0
        );

        // 集合成员：已在集合里的 1 号不算新增，其余两张才算。
        assert_eq!(
            count_membership_changes(&database, 1, &ids, true).unwrap(),
            2
        );
        assert_eq!(
            count_membership_changes(&database, 1, &ids, false).unwrap(),
            1
        );
    }

    #[test]
    fn missing_collection_is_reported_before_queueing() {
        let (_temp, database) = fixture();
        assert!(ensure_collection_exists(&database, 1).is_ok());
        let error = ensure_collection_exists(&database, 99).unwrap_err();
        assert!(error.to_string().contains("集合不存在"));
    }

    #[test]
    fn preview_counts_add_up_to_selected() {
        let (_temp, database) = fixture();
        let inspection = Inspection {
            database,
            root: PathBuf::from("."),
            selected: 5,
            active: vec![1, 2, 3],
            trashed: vec![4],
            missing: vec![5],
            sample: Vec::new(),
        };
        // skipped 同时包含不可用的图片与“已经是目标状态”的图片。
        let preview = sample_preview(&inspection, 2);
        assert_eq!(preview["selected"], 5);
        assert_eq!(preview["willChange"], 2);
        assert_eq!(preview["skipped"], 3);
    }

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

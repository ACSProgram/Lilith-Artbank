//! AI 服务与语义检索：协议、批量特征、语义搜索与相似分组。
//!
//! 只连 `settings.ai.baseUrl` 指向的本机服务；测试进程内的假服务即可覆盖协议与错误路径，
//! 不加载模型，也不启动 Python。

use std::cell::Cell;

use serde::Deserialize;
use serde_json::{Value, json};
use tauri::Manager;

use crate::{
    ai::{client::AiClient, job::AiJobKind},
    app::{
        error::{AppError, AppResult},
        state::AppState,
        tasks::{BatchTaskKind, BatchTaskRequest, ConflictPolicy, TaskScope},
        trace,
    },
    import::ImportRequest,
    library::{
        capability,
        query::{LibraryQuery, LibraryView, SortSpec},
    },
};

use super::{Options, value_of};

pub(super) fn dispatch(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let state = state.inner();
    let settings = state.settings.snapshot().ai;
    match options.action.as_deref() {
        Some("status") => value_of(Ok(state.ai.status(&settings))),
        Some("health") => value_of(client(&settings.base_url).health().map_err(describe)),
        Some("ensure") => {
            let request: EnsureRequest = options.request()?;
            value_of(
                client(&settings.base_url)
                    .ensure_models(&request.capabilities)
                    .map_err(describe),
            )
        }
        Some("embed-text") => {
            let request: EmbedTextRequest = options.request()?;
            let embedding = client(&settings.base_url)
                .embed_text(&request.model, &request.text)
                .map_err(describe)?;
            Ok(json!({
                "model": embedding.model,
                "dim": embedding.dim,
                "vector": embedding.vector,
            }))
        }
        Some("shutdown") => {
            client(&settings.base_url).shutdown().map_err(describe)?;
            Ok(json!({ "shutdown": true }))
        }
        Some("embed") => {
            let request: BatchRequest = options.request()?;
            let task = super::maintenance::run_batch(
                app,
                request.batch(BatchTaskKind::AiEmbedding),
                cancelled,
            )?;
            Ok(json!({
                "task": task,
                "features": features(state).map_err(|error| error.to_string())?,
            }))
        }
        Some("pose") => {
            let request: BatchRequest = options.request()?;
            let task = super::maintenance::run_batch(
                app,
                request.batch(BatchTaskKind::AiPose),
                cancelled,
            )?;
            Ok(json!({
                "task": task,
                "features": features(state).map_err(|error| error.to_string())?,
            }))
        }
        Some("pose-get") => {
            let request: ImageIdsRequest = options.request()?;
            let mut poses = Vec::with_capacity(request.image_ids.len());
            for image_id in request.image_ids {
                let pose = AppError::command(state.image_pose(image_id))
                    .map_err(|error| error.to_string())?;
                poses.push(json!({ "imageId": image_id, "pose": pose }));
            }
            Ok(json!({ "poses": poses }))
        }
        Some("describe") => {
            let request: DescribeRequest = options.request()?;
            let result = state
                .ai
                .describe_image(
                    &settings,
                    request.image_id,
                    request.prompt.as_deref(),
                    request.max_tokens,
                )
                .map_err(|error| error.to_string())?;
            state.after_history_write();
            value_of(Ok(result))
        }
        Some("search") => {
            let request: SearchRequest = options.request()?;
            value_of(
                state
                    .ai
                    .semantic_search(
                        &settings,
                        &request.text,
                        request.limit,
                        request.scope.as_ref(),
                        request.min_score,
                        None,
                    )
                    .map_err(|error| error.to_string()),
            )
        }
        Some("similar") => {
            let request: SimilarRequest = options.request()?;
            value_of(
                state
                    .ai
                    .find_similar_images(
                        &request.kind,
                        &request.target_ids,
                        request.threshold,
                        request.limit,
                        request.scope.as_ref(),
                    )
                    .map_err(|error| error.to_string()),
            )
        }
        Some("groups") => {
            let request: GroupsRequest = options.request()?;
            let task = super::maintenance::run_batch(
                app,
                BatchTaskRequest {
                    kind: BatchTaskKind::SimilarityGroups,
                    scope: request.scope,
                    group_phash: request.group_phash,
                    phash_threshold: request.phash_threshold,
                    group_embedding: request.group_embedding,
                    embedding_threshold: request.embedding_threshold,
                    conflict: request.conflict,
                    ..BatchTaskRequest::default()
                },
                cancelled,
            )?;
            let groups = state
                .similarity_groups_result(&task.task_id)
                .ok_or_else(|| "相似分组任务已结束但没有结果".to_owned())?;
            value_of(Ok(groups))
        }
        Some("keeps") => {
            let request: KeepsRequest = options.request()?;
            value_of(
                state
                    .similarity_group_keeps(&request.groups, &request.sort)
                    .map_err(|error| error.to_string()),
            )
        }
        Some("temp-features") => {
            let request: TempFeaturesRequest = options.request()?;
            let kind = match request.kind.trim().to_ascii_lowercase().as_str() {
                "embedding" => AiJobKind::Embedding,
                "pose" => AiJobKind::Pose,
                other => return Err(format!("临时区只支持 embedding/pose，收到 {other:?}")),
            };
            let start = trace::write("temp_features_generate", || {
                AppError::command(state.start_temp_features(kind, request.image_ids))
            })?;
            let task = super::maintenance::started_task(start)?;
            let task = super::wait_task(state, &task.task_id)?;
            super::check_task(&task, cancelled)?;
            Ok(json!({
                "task": task,
                "features": features(state).map_err(|error| error.to_string())?,
            }))
        }
        Some("features") => value_of(features(state).map_err(|error| error.to_string())),
        action => Err(match action {
            Some(action) => format!("未知命令：ai {action}"),
            None => "未知命令：ai".to_owned(),
        }),
    }
}

fn client(base_url: &str) -> AiClient {
    AiClient::new(base_url)
}

fn describe(error: crate::ai::client::AiError) -> String {
    error.to_string()
}

fn features(state: &AppState) -> AppResult<Value> {
    let summary = state.feature_summary()?;
    let (temp_embeddings, temp_poses) = match state.current_library() {
        Some((database, _)) => capability::temp_feature_summary(&database)?,
        None => (0, 0),
    };
    Ok(json!({
        "library": summary,
        "tempEmbeddings": temp_embeddings,
        "tempPoses": temp_poses,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnsureRequest {
    capabilities: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmbedTextRequest {
    model: String,
    text: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct BatchRequest {
    scope: TaskScope,
    overwrite: bool,
    conflict: ConflictPolicy,
}

impl BatchRequest {
    fn batch(self, kind: BatchTaskKind) -> BatchTaskRequest {
        BatchTaskRequest {
            kind,
            scope: self.scope,
            overwrite: self.overwrite,
            conflict: self.conflict,
            ..BatchTaskRequest::default()
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageIdsRequest {
    image_ids: Vec<i64>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DescribeRequest {
    image_id: i64,
    prompt: Option<String>,
    max_tokens: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SearchRequest {
    text: String,
    limit: Option<usize>,
    scope: Option<TaskScope>,
    min_score: Option<f64>,
}

/// 语义结果与相似结果只缓存在进程内；分页、全选与相似结果必须在同一个进程里跑完，
/// 所以用场景串起来，而不是拆成多个 CLI 进程。
pub(super) fn scenario(app: &tauri::AppHandle, options: &Options) -> Result<Value, String> {
    let request: SearchFlowRequest = options.request()?;
    let state = app.state::<AppState>();
    let state = state.inner();
    let settings = state.settings.snapshot().ai;
    let mut output = serde_json::Map::new();
    if let Some(text) = request
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        let search = state
            .ai
            .semantic_search(
                &settings,
                text,
                request.limit,
                request.scope.as_ref(),
                request.min_score,
                None,
            )
            .map_err(|error| error.to_string())?;
        let token = search.token.clone();
        let page = state
            .ai
            .similarity_page(&token, request.page_offset, request.page_limit)
            .map_err(|error| error.to_string())?;
        let ids = state
            .ai
            .similarity_ids(&token)
            .map_err(|error| error.to_string())?;
        output.insert(
            "search".into(),
            serde_json::to_value(search).map_err(|error| error.to_string())?,
        );
        output.insert(
            "page".into(),
            serde_json::to_value(page).map_err(|error| error.to_string())?,
        );
        output.insert(
            "ids".into(),
            serde_json::to_value(ids).map_err(|error| error.to_string())?,
        );
    }
    if let Some(similar) = request.similar {
        let result = state
            .ai
            .find_similar_images(
                &similar.kind,
                &similar.target_ids,
                similar.threshold,
                similar.limit,
                similar.scope.as_ref(),
            )
            .map_err(|error| error.to_string())?;
        let token = result.token.clone();
        let page = state
            .ai
            .similarity_page(&token, request.page_offset, request.page_limit)
            .map_err(|error| error.to_string())?;
        output.insert(
            "similar".into(),
            serde_json::to_value(result).map_err(|error| error.to_string())?,
        );
        output.insert(
            "similarPage".into(),
            serde_json::to_value(page).map_err(|error| error.to_string())?,
        );
    }
    if output.is_empty() {
        return Err("场景需要 text 或 similar".into());
    }
    Ok(Value::Object(output))
}

/// 临时区只在当前会话里存在：无头模式每条命令都会重新打开图库，所以导入、生成特征、
/// 搜索与关闭清理必须在同一个进程里完成。
pub(super) fn temp_scenario(
    app: &tauri::AppHandle,
    options: &Options,
    cancelled: &Cell<bool>,
) -> Result<Value, String> {
    let request: TempFlowRequest = options.request()?;
    let state = app.state::<AppState>();
    let state = state.inner();
    let import: ImportRequest = serde_json::from_value(json!({
        "paths": request.paths,
        "target": "temp",
        "duplicatePolicy": "skip",
    }))
    .map_err(|error| format!("场景请求无效：{error}"))?;
    let snapshot = trace::write("start_import", || {
        AppError::command(state.start_import(app.clone(), import))
    })?;
    let job_id = snapshot.progress.job_id.clone();
    let task = super::wait_task(state, &job_id)?;
    super::check_task(&task, cancelled)?;
    let counts_before = AppError::command(state.counts())?;
    let temp_ids = state
        .query_image_ids(LibraryQuery {
            view: LibraryView::Temp,
            ..LibraryQuery::default()
        })
        .map_err(|error| error.to_string())?;
    let kind = match request.kind.trim().to_ascii_lowercase().as_str() {
        "" | "embedding" => AiJobKind::Embedding,
        "pose" => AiJobKind::Pose,
        other => return Err(format!("临时区只支持 embedding/pose，收到 {other:?}")),
    };
    let start = trace::write("temp_features_generate", || {
        AppError::command(state.start_temp_features(kind, temp_ids.clone()))
    })?;
    let started = super::maintenance::started_task(start)?;
    let features_task = super::wait_task(state, &started.task_id)?;
    super::check_task(&features_task, cancelled)?;
    let (temp_embeddings, temp_poses) = match state.current_library() {
        Some((database, _)) => {
            capability::temp_feature_summary(&database).map_err(|error| error.to_string())?
        }
        None => (0, 0),
    };
    let settings = state.settings.snapshot().ai;
    let temp_scope = TaskScope::Temp;
    let temp_search = state
        .ai
        .semantic_search(
            &settings,
            "a red cloak",
            None,
            Some(&temp_scope),
            None,
            None,
        )
        .map_err(|error| error.to_string())?;
    let library_scope = TaskScope::Library;
    let library_search = state
        .ai
        .semantic_search(
            &settings,
            "a red cloak",
            None,
            Some(&library_scope),
            None,
            None,
        )
        .map_err(|error| error.to_string())?;
    let path = state
        .status()
        .map_err(|error| error.to_string())?
        .map(|info| info.path)
        .ok_or_else(|| "临时区场景需要已打开图库".to_owned())?;
    state.close_library().map_err(|error| error.to_string())?;
    state
        .open_library(std::path::Path::new(&path))
        .map_err(|error| error.to_string())?;
    let counts_after_reopen = AppError::command(state.counts())?;
    Ok(json!({
        "task": task,
        "countsBefore": counts_before,
        "tempIds": temp_ids,
        "featuresTask": features_task,
        "tempEmbeddings": temp_embeddings,
        "tempPoses": temp_poses,
        "tempSearch": temp_search,
        "librarySearch": library_search,
        "countsAfterReopen": counts_after_reopen,
    }))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct TempFlowRequest {
    paths: Vec<String>,
    kind: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SearchFlowRequest {
    text: Option<String>,
    limit: Option<usize>,
    scope: Option<TaskScope>,
    min_score: Option<f64>,
    page_offset: usize,
    page_limit: Option<usize>,
    similar: Option<SimilarRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimilarRequest {
    kind: String,
    #[serde(default)]
    target_ids: Vec<i64>,
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    scope: Option<TaskScope>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GroupsRequest {
    scope: TaskScope,
    group_phash: bool,
    phash_threshold: Option<u32>,
    group_embedding: bool,
    embedding_threshold: Option<f32>,
    conflict: ConflictPolicy,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeepsRequest {
    groups: Vec<Vec<i64>>,
    #[serde(default)]
    sort: Vec<SortSpec>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TempFeaturesRequest {
    kind: String,
    image_ids: Vec<i64>,
}

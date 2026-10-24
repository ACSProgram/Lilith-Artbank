use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use rusqlite::{params_from_iter, types::Value};
use serde::{Deserialize, Serialize};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::AiSettings,
        tasks::{TaskHandle, TaskKind, TaskPhase},
    },
    library::{content, db::LibraryDb, metadata, placeholders, thumbs::ThumbPack},
};

use super::{
    client::{AiClient, AiError, Capability, Health},
    embedding, input,
    pack::FeatureStore,
    pose,
};

/// 连续失败达到该数量即判定服务不可用，停止任务，避免把整库都记成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;
const CHUNK: usize = 400;

/// 描述长度未指定时使用的上限，与服务端 `/v1/describe` 的 `maxTokens` 约定一致。
pub const DEFAULT_DESCRIBE_TOKENS: u32 = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AiJobKind {
    #[default]
    Embedding,
    Pose,
    Description,
}

impl AiJobKind {
    pub fn capability(self) -> &'static str {
        match self {
            Self::Embedding => "imageEmbedding",
            Self::Pose => "pose",
            Self::Description => "description",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
            Self::Pose => "姿势",
            Self::Description => "描述",
        }
    }

    pub fn task_kind(self) -> TaskKind {
        match self {
            Self::Embedding => TaskKind::AiEmbedding,
            Self::Pose => TaskKind::AiPose,
            Self::Description => TaskKind::AiDescription,
        }
    }
}

/// 已解析成图片 id 的 AI 批量任务参数；范围由 `TaskScope` 解析后填入。
#[derive(Debug, Clone, Default)]
pub struct AiJobRequest {
    pub kind: AiJobKind,
    pub image_ids: Vec<i64>,
    pub overwrite: bool,
    pub prompt: Option<String>,
    pub max_tokens: Option<u32>,
}

impl AiJobRequest {
    pub fn normalized(mut self) -> AppResult<Self> {
        if let Some(prompt) = &mut self.prompt {
            *prompt = prompt.trim().to_owned();
            if prompt.chars().count() > 2_000 {
                return Err(AppError::message("描述提示词不能超过 2000 个字符"));
            }
        }
        if let Some(max_tokens) = self.max_tokens
            && !(16..=512).contains(&max_tokens)
        {
            return Err(AppError::message("描述长度必须在 16-512 之间"));
        }
        Ok(self)
    }
}

pub struct JobContext {
    pub database: Arc<LibraryDb>,
    pub thumbs: Arc<ThumbPack>,
    pub root: PathBuf,
    pub store: Arc<FeatureStore>,
    pub settings: AiSettings,
}

/// 任务主流程：探活并确保能力 → 过滤缺失项 → 逐张处理。
/// 取消只在图片之间生效；单张失败记入清单，连续失败达到上限才中止。
/// 进度、取消与失败清单都由 `TaskHandle` 承载，这里不再保留第二套状态。
pub fn execute(context: &JobContext, task: &TaskHandle, request: &AiJobRequest) -> AppResult<()> {
    let client = AiClient::new(&context.settings.base_url);
    task.set_phase(TaskPhase::Preparing);
    task.set_message(format!("正在准备{}模型…", request.kind.label()));
    let health = client
        .health()
        .map_err(|error| AppError::message(format!("AI 服务不可用：{error}")))?;
    let capability =
        ensure_capability(&client, &health, request.kind.capability()).map_err(|error| {
            AppError::message(format!("无法加载{}模型：{error}", request.kind.label()))
        })?;

    let ids = filter_missing(
        &context.database,
        request.image_ids.clone(),
        request,
        request.kind,
        &capability,
    )?;
    task.set_total(ids.len());
    task.set_phase(TaskPhase::Running);
    task.set_message("");
    if ids.is_empty() {
        return Ok(());
    }

    let mut consecutive_failures = 0_usize;
    for image_id in ids {
        if task.should_cancel() {
            return Ok(());
        }
        task.set_current(Some(image_id));
        let result = process_one(
            context,
            request.kind,
            &client,
            &capability,
            request,
            image_id,
        );
        match result {
            Ok(Outcome::Done) => {
                task.inc_completed();
                consecutive_failures = 0;
            }
            Ok(Outcome::Skipped) => {
                task.inc_skipped();
                consecutive_failures = 0;
            }
            Err(error) => {
                task.inc_failed();
                task.add_failure(Some(image_id), "", error.to_string());
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    return Err(AppError::message(format!(
                        "AI 服务连续失败 {consecutive_failures} 次，任务已停止：{error}"
                    )));
                }
            }
        }
    }
    Ok(())
}

enum Outcome {
    Done,
    Skipped,
}

fn process_one(
    context: &JobContext,
    kind: AiJobKind,
    client: &AiClient,
    capability: &Capability,
    request: &AiJobRequest,
    image_id: i64,
) -> AppResult<Outcome> {
    let row = content::load(&context.database, image_id)?;
    if row.deleted_at.is_some() {
        return Ok(Outcome::Skipped);
    }
    match kind {
        AiJobKind::Embedding => {
            let bytes =
                input::small_bytes(&context.database, &context.thumbs, &context.root, image_id)?;
            let result = client
                .embed_image(&capability.model, &bytes)
                .map_err(|error| AppError::message(error.to_string()))?;
            if result.model != capability.model {
                return Err(AppError::message(format!(
                    "服务返回的 embedding 模型 {0} 与 health 中的 {1} 不一致",
                    result.model, capability.model
                )));
            }
            embedding::store(
                &context.database,
                &context.store,
                image_id,
                &result.model,
                result.dim,
                &result.vector,
            )?;
            Ok(Outcome::Done)
        }
        AiJobKind::Pose => {
            let bytes =
                input::small_bytes(&context.database, &context.thumbs, &context.root, image_id)?;
            let result = client
                .pose(&capability.model, &bytes)
                .map_err(|error| AppError::message(error.to_string()))?;
            if result.model.trim().is_empty() {
                return Err(AppError::message("姿势服务没有返回模型名"));
            }
            pose::store(&context.database, &context.store, image_id, &result)?;
            Ok(Outcome::Done)
        }
        AiJobKind::Description => {
            let bytes = input::description_bytes(&context.database, &context.root, image_id)?;
            let result = client
                .describe(
                    request.prompt.as_deref().unwrap_or(""),
                    Some(request.max_tokens.unwrap_or(DEFAULT_DESCRIBE_TOKENS)),
                    &bytes,
                )
                .map_err(|error| AppError::message(error.to_string()))?;
            if result.text.trim().is_empty() {
                return Err(AppError::message("描述服务返回了空文本"));
            }
            let changed = metadata::set_ai_description(
                &context.database,
                image_id,
                &result.text,
                &result.model,
            )?;
            Ok(if changed {
                Outcome::Done
            } else {
                Outcome::Skipped
            })
        }
    }
}

pub(crate) fn ensure_capability(
    client: &AiClient,
    health: &Health,
    name: &str,
) -> Result<Capability, AiError> {
    if let Some(capability) = health.capability(name)
        && capability.ready
    {
        return Ok(capability.clone());
    }
    let capabilities = client.ensure_models(&[name.to_owned()])?;
    if let Some(capability) = capabilities.get(name)
        && capability.ready
    {
        return Ok(capability.clone());
    }
    let refreshed = client.health()?;
    if let Some(capability) = refreshed.capability(name)
        && capability.ready
    {
        return Ok(capability.clone());
    }
    Err(AiError {
        message: format!("服务没有可用的 {name} 能力"),
        retryable: false,
    })
}

fn filter_missing(
    database: &LibraryDb,
    ids: Vec<i64>,
    request: &AiJobRequest,
    kind: AiJobKind,
    capability: &Capability,
) -> AppResult<Vec<i64>> {
    if request.overwrite {
        return Ok(match kind {
            AiJobKind::Description => filter_user_descriptions(database, ids)?,
            _ => ids,
        });
    }
    let existing = match kind {
        AiJobKind::Embedding => {
            let dim = capability.dim.ok_or_else(|| {
                AppError::message("health 没有返回 embedding 维度，无法判断缺失项")
            })?;
            existing_features(database, &ids, "embedding", Some((&capability.model, dim)))?
        }
        AiJobKind::Pose => existing_features(database, &ids, "pose", None)?,
        AiJobKind::Description => described_ids(database, &ids)?,
    };
    Ok(ids
        .into_iter()
        .filter(|id| !existing.contains(id))
        .collect())
}

fn filter_user_descriptions(database: &LibraryDb, ids: Vec<i64>) -> AppResult<Vec<i64>> {
    let states = description_states(database, &ids)?;
    Ok(ids
        .into_iter()
        .filter(|id| !states.get(id).is_some_and(|state| state.is_user))
        .collect())
}

fn existing_features(
    database: &LibraryDb,
    ids: &[i64],
    kind: &str,
    model_dim: Option<(&str, u32)>,
) -> AppResult<HashSet<i64>> {
    let mut found = HashSet::new();
    for chunk in ids.chunks(CHUNK) {
        let rows = database.with_reader(|connection| {
            let placeholders = placeholders(chunk.len());
            let (sql, mut params) = match model_dim {
                Some((model, dim)) => (
                    format!(
                        "SELECT image_id FROM feature_blobs
                         WHERE kind = ?1 AND model = ?2 AND dim = ?3
                           AND image_id IN ({placeholders})"
                    ),
                    vec![
                        Value::Text(kind.to_owned()),
                        Value::Text(model.to_owned()),
                        Value::Integer(i64::from(dim)),
                    ],
                ),
                None => (
                    format!(
                        "SELECT image_id FROM feature_blobs
                         WHERE kind = ?1 AND image_id IN ({placeholders})"
                    ),
                    vec![Value::Text(kind.to_owned())],
                ),
            };
            params.extend(chunk.iter().map(|id| Value::Integer(*id)));
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(params_from_iter(params.iter()), |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        found.extend(rows);
    }
    Ok(found)
}

/// 描述文本与来源；来源为 `user` 时 AI 不得覆盖。
struct DescriptionState {
    text: String,
    is_user: bool,
}

fn described_ids(database: &LibraryDb, ids: &[i64]) -> AppResult<HashSet<i64>> {
    Ok(description_states(database, ids)?
        .into_iter()
        .filter(|(_, state)| !state.text.trim().is_empty())
        .map(|(id, _)| id)
        .collect())
}

fn description_states(
    database: &LibraryDb,
    ids: &[i64],
) -> AppResult<HashMap<i64, DescriptionState>> {
    let mut states = HashMap::new();
    for chunk in ids.chunks(CHUNK) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT id, description, description_source FROM images WHERE id IN ({placeholders})"
        );
        let values: Vec<Value> = chunk.iter().map(|id| Value::Integer(*id)).collect();
        database.with_reader(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(params_from_iter(values.iter()), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, text, source) in rows {
                states.insert(
                    id,
                    DescriptionState {
                        text,
                        is_user: source == "user",
                    },
                );
            }
            Ok(())
        })?;
    }
    Ok(states)
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;

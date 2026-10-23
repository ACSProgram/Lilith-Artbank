use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use parking_lot::Mutex;
use rusqlite::params_from_iter;
use serde::{Deserialize, Serialize};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::AiSettings,
    },
    library::{
        content,
        db::LibraryDb,
        metadata, placeholders,
        query::{self, LibraryQuery},
        service::now_ms,
        thumbs::ThumbPack,
    },
};

use super::{
    client::{AiClient, AiError, Capability, Health},
    embedding, input,
    pack::FeatureStore,
    pose,
};

const MAX_FAILURES: usize = 200;
const MAX_IDS: usize = 1_000_000;
const MAX_CONSECUTIVE_FAILURES: usize = 5;
const CHUNK: usize = 400;

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

    fn label(self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
            Self::Pose => "姿势",
            Self::Description => "描述",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AiJobPhase {
    Preparing,
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiJobFailure {
    pub image_id: Option<i64>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiJobProgress {
    pub job_id: String,
    pub kind: AiJobKind,
    pub phase: AiJobPhase,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub current_image_id: Option<i64>,
    pub message: String,
    pub elapsed_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiJobSnapshot {
    #[serde(flatten)]
    pub progress: AiJobProgress,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub failures: Vec<AiJobFailure>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AiJobRequest {
    pub kind: AiJobKind,
    pub image_ids: Vec<i64>,
    pub query: Option<LibraryQuery>,
    pub overwrite: bool,
    pub prompt: Option<String>,
    pub max_tokens: Option<u32>,
}

impl AiJobRequest {
    pub fn normalized(mut self) -> AppResult<Self> {
        self.image_ids.retain(|id| *id > 0);
        self.image_ids.sort_unstable();
        self.image_ids.dedup();
        if self.image_ids.len() > MAX_IDS {
            return Err(AppError::message("单次 AI 任务最多处理 100 万张图片"));
        }
        if !self.image_ids.is_empty() {
            self.query = None;
        }
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

pub struct AiJob {
    id: String,
    kind: AiJobKind,
    cancel: AtomicBool,
    running: AtomicBool,
    phase: Mutex<AiJobPhase>,
    total: AtomicUsize,
    completed: AtomicUsize,
    failed: AtomicUsize,
    skipped: AtomicUsize,
    current: Mutex<Option<i64>>,
    message: Mutex<String>,
    failures: Mutex<Vec<AiJobFailure>>,
    started_at: i64,
    finished_at: Mutex<Option<i64>>,
}

impl AiJob {
    pub fn new(id: impl Into<String>, kind: AiJobKind) -> Self {
        Self {
            id: id.into(),
            kind,
            cancel: AtomicBool::new(false),
            running: AtomicBool::new(true),
            phase: Mutex::new(AiJobPhase::Preparing),
            total: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            current: Mutex::new(None),
            message: Mutex::new(String::new()),
            failures: Mutex::new(Vec::new()),
            started_at: now_ms(),
            finished_at: Mutex::new(None),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> AiJobKind {
        self.kind
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn set_phase(&self, phase: AiJobPhase) {
        *self.phase.lock() = phase;
    }

    pub fn set_message(&self, message: impl Into<String>) {
        *self.message.lock() = message.into();
    }

    fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
    }

    fn set_current(&self, image_id: Option<i64>) {
        *self.current.lock() = image_id;
    }

    fn inc_completed(&self) {
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    fn add_failure(&self, image_id: Option<i64>, reason: impl Into<String>) {
        let mut failures = self.failures.lock();
        if failures.len() < MAX_FAILURES {
            failures.push(AiJobFailure {
                image_id,
                reason: reason.into(),
            });
        }
    }

    pub fn finish(&self, phase: AiJobPhase) {
        self.set_phase(phase);
        self.set_current(None);
        *self.finished_at.lock() = Some(now_ms());
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn progress(&self) -> AiJobProgress {
        AiJobProgress {
            job_id: self.id.clone(),
            kind: self.kind,
            phase: *self.phase.lock(),
            total: self.total.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            current_image_id: *self.current.lock(),
            message: self.message.lock().clone(),
            elapsed_ms: now_ms().saturating_sub(self.started_at),
        }
    }

    pub fn snapshot(&self) -> AiJobSnapshot {
        AiJobSnapshot {
            progress: self.progress(),
            started_at: self.started_at,
            finished_at: *self.finished_at.lock(),
            failures: self.failures.lock().clone(),
        }
    }
}

pub fn execute(context: &JobContext, job: &AiJob, request: &AiJobRequest) -> AppResult<()> {
    let client = AiClient::new(&context.settings.base_url);
    job.set_phase(AiJobPhase::Preparing);
    job.set_message(format!("正在准备{}模型…", job.kind().label()));
    let health = client
        .health()
        .map_err(|error| AppError::message(format!("AI 服务不可用：{error}")))?;
    let capability =
        ensure_capability(&client, &health, job.kind().capability()).map_err(|error| {
            AppError::message(format!("无法加载{}模型：{error}", job.kind().label()))
        })?;

    let ids = resolve_ids(&context.database, request)?;
    let ids = filter_missing(&context.database, ids, request, job.kind(), &capability)?;
    job.set_total(ids.len());
    job.set_phase(AiJobPhase::Running);
    job.set_message(String::new());
    if ids.is_empty() {
        return Ok(());
    }

    let mut consecutive_failures = 0_usize;
    for image_id in ids {
        if job.should_cancel() {
            return Ok(());
        }
        job.set_current(Some(image_id));
        let result = process_one(context, job.kind(), &client, &capability, request, image_id);
        match result {
            Ok(Outcome::Done) => {
                job.inc_completed();
                consecutive_failures = 0;
            }
            Ok(Outcome::Skipped) => {
                job.inc_skipped();
                consecutive_failures = 0;
            }
            Err(error) => {
                job.inc_failed();
                job.add_failure(Some(image_id), error.to_string());
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
                    request.max_tokens.or(Some(160)),
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

fn resolve_ids(database: &LibraryDb, request: &AiJobRequest) -> AppResult<Vec<i64>> {
    if !request.image_ids.is_empty() {
        return Ok(request.image_ids.clone());
    }
    if let Some(query) = &request.query {
        return query::query_ids(database, query.clone());
    }
    database.with_reader(|connection| {
        let mut statement = connection
            .prepare("SELECT id FROM images WHERE deleted_at IS NULL ORDER BY id LIMIT ?1")?;
        let ids = statement
            .query_map([MAX_IDS as i64 + 1], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.len() > MAX_IDS {
            return Err(AppError::message(
                "图库图片超过 100 万张，请先筛选后再运行 AI 任务",
            ));
        }
        Ok(ids)
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
    match kind {
        AiJobKind::Embedding => {
            let dim = capability.dim.ok_or_else(|| {
                AppError::message("health 没有返回 embedding 维度，无法判断缺失项")
            })?;
            let existing =
                existing_features(database, &ids, "embedding", Some((&capability.model, dim)))?;
            Ok(ids
                .into_iter()
                .filter(|id| !existing.contains(id))
                .collect())
        }
        AiJobKind::Pose => {
            let existing = existing_features(database, &ids, "pose", None)?;
            Ok(ids
                .into_iter()
                .filter(|id| !existing.contains(id))
                .collect())
        }
        AiJobKind::Description => {
            let described = described_ids(database, &ids)?;
            Ok(ids
                .into_iter()
                .filter(|id| !described.contains(id))
                .collect())
        }
    }
}

fn filter_user_descriptions(database: &LibraryDb, ids: Vec<i64>) -> AppResult<Vec<i64>> {
    let states = description_states(database, &ids)?;
    Ok(ids
        .into_iter()
        .filter(|id| !states.get(id).is_some_and(|(_, source)| source == "user"))
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
        let set = database.with_reader(|connection| {
            let placeholders = placeholders(chunk.len());
            let sql = if model_dim.is_some() {
                format!(
                    "SELECT image_id FROM feature_blobs
                     WHERE kind = '{kind}' AND model = ? AND dim = ?
                       AND image_id IN ({placeholders})"
                )
            } else {
                format!(
                    "SELECT image_id FROM feature_blobs
                     WHERE kind = '{kind}' AND image_id IN ({placeholders})"
                )
            };
            let mut values: Vec<rusqlite::types::Value> = chunk
                .iter()
                .map(|id| rusqlite::types::Value::Integer(*id))
                .collect();
            let mut statement = connection.prepare(&sql)?;
            let rows = if let Some((model, dim)) = model_dim {
                let mut params: Vec<rusqlite::types::Value> = vec![
                    rusqlite::types::Value::Text(model.to_owned()),
                    rusqlite::types::Value::Integer(i64::from(dim)),
                ];
                params.append(&mut values);
                statement
                    .query_map(params_from_iter(params.iter()), |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                statement
                    .query_map(params_from_iter(values.iter()), |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            Ok(rows)
        })?;
        found.extend(set);
    }
    Ok(found)
}

fn described_ids(database: &LibraryDb, ids: &[i64]) -> AppResult<HashSet<i64>> {
    Ok(description_states(database, ids)?
        .into_iter()
        .filter(|(_, (text, _))| !text.trim().is_empty())
        .map(|(id, _)| id)
        .collect())
}

fn description_states(
    database: &LibraryDb,
    ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, (String, String)>> {
    let mut states = std::collections::HashMap::new();
    for chunk in ids.chunks(CHUNK) {
        let placeholders = placeholders(chunk.len());
        let sql = format!(
            "SELECT id, description, description_source FROM images WHERE id IN ({placeholders})"
        );
        let values: Vec<rusqlite::types::Value> = chunk
            .iter()
            .map(|id| rusqlite::types::Value::Integer(*id))
            .collect();
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
                states.insert(id, (text, source));
            }
            Ok(())
        })?;
    }
    Ok(states)
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;

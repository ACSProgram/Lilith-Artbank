use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::AiSettings,
        tasks::{TaskHandle, TaskKind, TaskPhase},
    },
    concurrency::{
        budget::ImageBudget,
        resources::{HolderInfo, ResourceTable},
    },
    library::{
        content,
        db::LibraryDb,
        history::{self, HistoryKind, NewOperation},
        metadata,
        query::{self, PendingFilter},
        thumbs::ThumbPack,
    },
};

use super::{
    client::{AiClient, AiError, Capability, Health},
    embedding, input,
    pack::FeatureStore,
    pose,
};

/// 连续失败达到该数量即判定服务不可用，停止任务，避免把整库都记成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;

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
    pub resources: Arc<ResourceTable>,
    pub images: Arc<ImageBudget>,
}

/// 任务主流程：探活并确保能力 → 过滤缺失项 → 逐张处理。
/// 取消在图片之间生效，单次请求进行中也能立刻打断；单张失败记入清单，连续失败达到上限才中止。
/// 进度、取消与失败清单都由 `TaskHandle` 承载，这里不再保留第二套状态。
pub fn execute(
    context: &JobContext,
    task: &Arc<TaskHandle>,
    request: &AiJobRequest,
) -> AppResult<()> {
    let cancel_task = task.clone();
    let client = AiClient::cancellable(
        &context.settings.base_url,
        Arc::new(move || cancel_task.should_cancel()),
    );
    task.set_phase(TaskPhase::Preparing);
    task.set_message(format!("正在准备{}模型…", request.kind.label()));
    let health = client
        .health()
        .map_err(|error| AppError::message(format!("AI 服务不可用：{error}")))?;
    let capability =
        ensure_capability(&client, &health, request.kind.capability()).map_err(|error| {
            AppError::message(format!("无法加载{}模型：{error}", request.kind.label()))
        })?;

    let ids = {
        let filter = match (request.kind, request.overwrite) {
            (AiJobKind::Embedding, false) => {
                let dim = capability.dim.ok_or_else(|| {
                    AppError::message("health 没有返回 embedding 维度，无法判断缺失项")
                })?;
                PendingFilter::Embedding {
                    model: capability.model.clone(),
                    dim,
                }
            }
            (AiJobKind::Pose, false) => PendingFilter::Pose,
            (AiJobKind::Description, false) => PendingFilter::Description { overwrite: false },
            (AiJobKind::Description, true) => PendingFilter::Description { overwrite: true },
            _ => PendingFilter::All,
        };
        task.set_message("正在统计待处理图片…");
        query::selected_pending_ids(&context.database, &request.image_ids, &filter)?
    };
    task.set_total(ids.len());
    task.set_phase(TaskPhase::Running);
    task.set_message("");
    if ids.is_empty() {
        return Ok(());
    }

    // 描述任务会产生用户可见的内容修改：先建 live 记录，逐张追加旧描述；
    // 取消、失败或 panic 时已经写入的描述仍然可以撤销。
    let operation = if request.kind == AiJobKind::Description {
        Some(history::LiveOperation::begin(
            &context.database,
            NewOperation::new(
                HistoryKind::AiDescription,
                format!("AI 描述 {} 张", ids.len()),
            ),
        )?)
    } else {
        None
    };
    let operation_id = operation.as_ref().map(history::LiveOperation::id);

    let outcome = run_job(
        context,
        task,
        request,
        &client,
        &capability,
        ids,
        operation_id,
    );
    if let Some(operation) = operation {
        let error = outcome
            .as_ref()
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        if let Err(error) = operation.finish(&error) {
            log::warn!("收尾 AI 描述记录失败：{error}");
        }
    }
    outcome
}

fn run_job(
    context: &JobContext,
    task: &TaskHandle,
    request: &AiJobRequest,
    client: &AiClient,
    capability: &Capability,
    ids: Vec<i64>,
    operation_id: Option<i64>,
) -> AppResult<()> {
    let holder = HolderInfo::new(task.id(), request.kind.label()).with_path_note("正在读取");
    let cancel = || task.should_cancel();
    let mut consecutive_failures = 0_usize;
    for image_id in ids {
        if cancel() {
            return Ok(());
        }
        task.set_current(Some(image_id));
        let result = process_one(
            context,
            task,
            request.kind,
            client,
            capability,
            request,
            image_id,
            operation_id,
            &holder,
            &cancel,
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
                // 取消打断的请求不算失败：用户已经不要结果了，不该在失败清单里留一条。
                if cancel() {
                    return Ok(());
                }
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

#[allow(clippy::too_many_arguments)]
fn process_one(
    context: &JobContext,
    task: &TaskHandle,
    kind: AiJobKind,
    client: &AiClient,
    capability: &Capability,
    request: &AiJobRequest,
    image_id: i64,
    operation_id: Option<i64>,
    holder: &HolderInfo,
    cancel: &dyn Fn() -> bool,
) -> AppResult<Outcome> {
    let row = content::load(&context.database, image_id)?;
    if row.deleted_at.is_some() {
        return Ok(Outcome::Skipped);
    }
    let path = row.relative_display_path();
    task.set_current_file(Some(&path));
    match kind {
        AiJobKind::Embedding => {
            let bytes = input::small_bytes(
                &context.database,
                &context.thumbs,
                &context.root,
                image_id,
                &context.resources,
                &context.images,
                holder,
                cancel,
            )?;
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
            let bytes = input::small_bytes(
                &context.database,
                &context.thumbs,
                &context.root,
                image_id,
                &context.resources,
                &context.images,
                holder,
                cancel,
            )?;
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
            let bytes = input::description_bytes(
                &context.database,
                &context.root,
                image_id,
                &context.resources,
                &context.images,
                holder,
                cancel,
            )?;
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
            let changed = if let Some(operation_id) = operation_id {
                metadata::set_ai_description_live(
                    &context.database,
                    operation_id,
                    image_id,
                    &result.text,
                    &result.model,
                )?
            } else {
                metadata::set_ai_description(
                    &context.database,
                    image_id,
                    &result.text,
                    &result.model,
                )?
            };
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
        transport: false,
        cancelled: false,
    })
}

#[cfg(test)]
#[path = "job_tests.rs"]
mod tests;

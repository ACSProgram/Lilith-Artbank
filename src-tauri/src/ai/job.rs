use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::AiSettings,
        tasks::{TaskHandle, TaskKind, TaskPhase},
    },
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget},
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
    basis::{self, ActiveBasis},
    client::{AiClient, AiError, Capability, Health},
    descriptors, embedding, input,
    pack::{FeatureKind, FeatureStore, semantic_feature_demands},
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
            Self::Embedding => "语义特征",
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
    pub memory: Arc<ImageMemoryBudget>,
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

    // 语义特征依赖库内白化基。小库直接用通用兜底；大库在第一次生成语义特征时
    // 才拟合专用基，开库时不跑重任务。取消或失败都不会切换 meta。
    let basis = if request.kind == AiJobKind::Embedding {
        match resolve_basis(context, task)? {
            Some(active) => Some(active),
            None => return Ok(()),
        }
    } else {
        None
    };
    if let Some(active) = &basis {
        let meta = basis::load_meta(&context.database)?;
        if meta.as_ref().is_some_and(|meta| meta.id != active.id) {
            task.set_message("检测到库内基已更换，正在重新生成语义描述子…");
        } else if meta
            .as_ref()
            .is_some_and(|meta| meta.source == basis::SOURCE_UNIVERSAL)
        {
            task.set_message("正在准备语义特征模型（使用通用兜底基）…");
        }
    }

    let capability = match &basis {
        Some(active) => ensure_image_capability(&client, &health, active).map_err(|error| {
            AppError::message(format!("无法加载{}模型：{error}", request.kind.label()))
        })?,
        None => {
            ensure_capability(&client, &health, request.kind.capability()).map_err(|error| {
                AppError::message(format!("无法加载{}模型：{error}", request.kind.label()))
            })?
        }
    };

    let ids = {
        let filter = match (request.kind, request.overwrite) {
            (AiJobKind::Embedding, false) => {
                let dim = capability.dim.ok_or_else(|| {
                    AppError::message("health 没有返回 embedding 维度，无法判断缺失项")
                })?;
                PendingFilter::Features {
                    model: capability.model.clone(),
                    dims: semantic_feature_demands(
                        &capability.model,
                        dim,
                        basis.as_ref().map(|basis| basis.id.as_str()),
                    ),
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
        basis.as_ref(),
        ids,
        operation_id,
    );
    // 只有真正跑完才切 meta：中途取消时旧基必须继续服务未处理的图片。
    let cancelled = task.should_cancel();
    if outcome.is_ok()
        && !cancelled
        && let Some(active) = &basis
    {
        if let Err(error) = basis::commit(
            &context.database,
            &context.root,
            active,
            capability.basis_samples,
        ) {
            log::warn!("写入语义特征基身份失败：{error}");
        }
        // 旧包可能还被本次进程的写句柄占着；关掉句柄后再释放一次。
        context.store.release_handles();
        if let Err(error) = basis::release_pending(&context.database, &context.root) {
            log::warn!("释放旧语义特征包失败：{error}");
        }
    }
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

/// 确定本次任务要写入的基；库内没有可用基时，在本任务开头拟合。
///
/// 返回 `None` 表示拟合期间被取消，任务按取消结束。
fn resolve_basis(context: &JobContext, task: &Arc<TaskHandle>) -> AppResult<Option<ActiveBasis>> {
    basis::prepare_local(&context.database, &context.settings, &context.root)?;
    if let Some(active) = basis::active_basis(&context.settings, &context.root)? {
        return Ok(Some(active));
    }
    let fit = basis::FitContext {
        database: context.database.clone(),
        thumbs: context.thumbs.clone(),
        root: context.root.clone(),
        settings: context.settings.clone(),
        resources: context.resources.clone(),
        images: context.images.clone(),
        memory: context.memory.clone(),
    };
    let fitted = basis::fit(&fit, task)?;
    // 拟合阶段按样本张数计数；后面要按待处理图片重新计数，先把这段进度清掉，
    // 否则统计完成前进度条会短暂显示拟合阶段的 100%。
    task.set_completed(0);
    task.set_total(0);
    Ok(fitted)
}

#[allow(clippy::too_many_arguments)]
fn run_job(
    context: &JobContext,
    task: &TaskHandle,
    request: &AiJobRequest,
    client: &AiClient,
    capability: &Capability,
    basis: Option<&ActiveBasis>,
    ids: Vec<i64>,
    operation_id: Option<i64>,
) -> AppResult<()> {
    let holder =
        HolderInfo::new(task.id(), request.kind.task_kind().label()).with_path_note("正在读取");
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
            basis.map(|basis| basis.id.as_str()),
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
    basis_id: Option<&str>,
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
                &context.memory,
                holder,
                cancel,
            )?;
            // 三种描述子出自同一次前向：缺什么补什么时也把三个都重写一遍，
            // embedding 是确定性结果，覆盖无害，换来「跑一次就自洽」。
            let result = client
                .embed_image_features(&capability.model, &["global", "spp", "maxsim"], &bytes)
                .map_err(|error| AppError::message(error.to_string()))?;
            if result.embedding.model != capability.model {
                return Err(AppError::message(format!(
                    "服务返回的 embedding 模型 {0} 与 health 中的 {1} 不一致",
                    result.embedding.model, capability.model
                )));
            }
            // 校验放在写之前：缺任一条就整张不写，下次仍是待处理，不会留下半张图。
            let spp = result
                .spp
                .ok_or_else(|| AppError::message("语义服务没有返回构图特征"))?;
            let maxsim = result
                .maxsim
                .ok_or_else(|| AppError::message("语义服务没有返回主体特征"))?;
            embedding::store(
                &context.database,
                &context.store,
                image_id,
                &result.embedding.model,
                result.embedding.dim,
                &result.embedding.vector,
            )?;
            descriptors::store(
                &context.database,
                &context.store,
                image_id,
                &result.embedding.model,
                basis_id,
                FeatureKind::Spp,
                &spp,
            )?;
            descriptors::store(
                &context.database,
                &context.store,
                image_id,
                &result.embedding.model,
                basis_id,
                FeatureKind::MaxSim,
                &maxsim,
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
                &context.memory,
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
                &context.memory,
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

/// 加载图片塔并确认它用的是本次任务要写的基。
///
/// 已加载但基不同时必须重载，否则描述子会混进旧基的包。
pub(crate) fn ensure_image_capability(
    client: &AiClient,
    health: &Health,
    basis: &ActiveBasis,
) -> Result<Capability, AiError> {
    if let Some(capability) = health.capability("imageEmbedding")
        && capability.ready
        && capability.basis_id.as_deref() == Some(basis.id.as_str())
    {
        return Ok(capability.clone());
    }
    let request = basis.paths.request(&basis.id);
    let capabilities =
        client.ensure_models_with_basis(&["imageEmbedding".to_owned()], Some(&request))?;
    let capability = capabilities.get("imageEmbedding").cloned().or_else(|| {
        client
            .health()
            .ok()
            .and_then(|health| health.capability("imageEmbedding").cloned())
    });
    let Some(capability) = capability else {
        return Err(AiError::invalid("服务没有可用的 imageEmbedding 能力"));
    };
    if !capability.ready {
        return Err(AiError::invalid("图片塔没有加载成功"));
    }
    if let Some(loaded) = capability.basis_id.as_deref()
        && loaded != basis.id
    {
        return Err(AiError::invalid(format!(
            "图片塔加载的基 {loaded} 与任务要求的 {} 不一致",
            basis.id
        )));
    }
    Ok(capability)
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

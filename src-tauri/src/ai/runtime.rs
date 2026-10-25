use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::AiSettings,
        state::AppState,
        tasks::{
            ConflictPolicy, LaneGuard, TaskBus, TaskHandle, TaskKind, TaskPhase, TaskScope,
            TaskStartResult,
        },
    },
    library::{
        db::LibraryDb,
        paths::LibraryPaths,
        query::{self, ImageRow},
        similarity::{self as library_similarity, GroupProgress},
        thumbs::ThumbPack,
        viewer::PoseData,
    },
};

use super::{
    client::{AiClient, Health},
    embedding,
    job::{self, AiJobKind, AiJobRequest, JobContext},
    pack::FeatureStore,
    pose,
    process::{AiProcess, MAX_START_ATTEMPTS, ProcessState, ProcessStatus, launch_spec},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MeanPhase {
    Missing,
    Computing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeanStatus {
    pub phase: MeanPhase,
    pub model: Option<String>,
    pub dim: Option<u32>,
    pub count: u64,
    pub message: Option<String>,
}

#[derive(Clone)]
struct MeanSlot {
    phase: MeanPhase,
    model: Option<String>,
    dim: Option<u32>,
    count: u64,
    vector: Option<Arc<Vec<f32>>>,
    message: Option<String>,
}

impl MeanSlot {
    fn missing(message: impl Into<String>) -> Self {
        Self::empty(None, None, message)
    }

    fn empty(model: Option<String>, dim: Option<u32>, message: impl Into<String>) -> Self {
        Self {
            phase: MeanPhase::Missing,
            model,
            dim,
            count: 0,
            vector: None,
            message: Some(message.into()),
        }
    }

    fn computing(model: Option<String>, dim: Option<u32>) -> Self {
        Self {
            phase: MeanPhase::Computing,
            model,
            dim,
            count: 0,
            vector: None,
            message: Some("正在计算 embedding 均值…".into()),
        }
    }

    fn ready(model: &str, dim: u32, mean: &embedding::MeanEmbedding) -> Self {
        Self {
            phase: MeanPhase::Ready,
            model: Some(model.to_owned()),
            dim: Some(dim),
            count: mean.count,
            vector: Some(Arc::new(mean.vector.clone())),
            message: None,
        }
    }

    fn failed(model: &str, dim: u32, message: impl Into<String>) -> Self {
        Self {
            phase: MeanPhase::Failed,
            model: Some(model.to_owned()),
            dim: Some(dim),
            count: 0,
            vector: None,
            message: Some(message.into()),
        }
    }

    fn status(&self) -> MeanStatus {
        MeanStatus {
            phase: self.phase,
            model: self.model.clone(),
            dim: self.dim,
            count: self.count,
            message: self.message.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiStatus {
    pub base_url: String,
    pub auto_start: bool,
    pub launch_configured: bool,
    pub online: bool,
    pub process: ProcessStatus,
    pub health: Option<Health>,
    pub mean: MeanStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSearchResult {
    pub token: String,
    pub model: String,
    pub dim: u32,
    pub total: usize,
    pub elapsed_ms: i64,
    pub mean_count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimilarityPage {
    pub items: Vec<ImageRow>,
    pub next_offset: Option<usize>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSimilarityResult {
    pub token: String,
    pub kind: String,
    pub total: usize,
    pub elapsed_ms: i64,
}

/// 相似分组的两种指标；可以单独启用，也可以同时启用（同时满足才归组）。
#[derive(Debug, Clone, Copy, Default)]
pub struct GroupOptions {
    pub phash: bool,
    pub phash_threshold: Option<u32>,
    pub embedding: bool,
    pub embedding_threshold: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimilarityGroupsResult {
    pub phash: bool,
    pub embedding: bool,
    pub groups: Vec<Vec<i64>>,
    pub truncated: bool,
    pub missing: usize,
    pub elapsed_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiDescriptionResult {
    pub image_id: i64,
    pub model: String,
    pub text: String,
}

struct LibraryHandles {
    generation: u64,
    library_id: String,
    database: Arc<LibraryDb>,
    thumbs: Arc<ThumbPack>,
    root: PathBuf,
}

struct ResultCache {
    token: String,
    generation: u64,
    library_id: String,
    ids: Vec<i64>,
}

struct Inner {
    process: Mutex<AiProcess>,
    mean: RwLock<MeanSlot>,
    results: Mutex<Option<ResultCache>>,
    visuals: RwLock<Option<Arc<library_similarity::VisualIndex>>>,
    groups_results: Mutex<HashMap<String, SimilarityGroupsResult>>,
    store: Arc<FeatureStore>,
    library: RwLock<Option<LibraryHandles>>,
    generation: AtomicU64,
    supervisor_generation: AtomicU64,
    supervising: AtomicBool,
    manual_stop: AtomicBool,
}

#[derive(Clone)]
pub struct AiRuntime {
    inner: Arc<Inner>,
}

impl Default for AiRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl AiRuntime {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                process: Mutex::new(AiProcess::default()),
                mean: RwLock::new(MeanSlot::missing("尚未打开图库")),
                results: Mutex::new(None),
                visuals: RwLock::new(None),
                groups_results: Mutex::new(HashMap::new()),
                store: Arc::new(FeatureStore::new()),
                library: RwLock::new(None),
                generation: AtomicU64::new(0),
                supervisor_generation: AtomicU64::new(0),
                supervising: AtomicBool::new(false),
                manual_stop: AtomicBool::new(false),
            }),
        }
    }

    pub fn library_opened(
        &self,
        library_id: String,
        database: Arc<LibraryDb>,
        thumbs: Arc<ThumbPack>,
        root: PathBuf,
    ) {
        let generation = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner.store.set_root(LibraryPaths::new(&root).features);
        *self.inner.library.write() = Some(LibraryHandles {
            generation,
            library_id,
            database: database.clone(),
            thumbs,
            root,
        });
        *self.inner.results.lock() = None;
        *self.inner.visuals.write() = None;
        self.inner.groups_results.lock().clear();
        *self.inner.mean.write() = MeanSlot::computing(None, None);
        self.spawn_mean(generation, database);
    }

    pub fn library_closed(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        *self.inner.library.write() = None;
        self.inner.store.clear();
        *self.inner.results.lock() = None;
        *self.inner.visuals.write() = None;
        self.inner.groups_results.lock().clear();
        *self.inner.mean.write() = MeanSlot::missing("尚未打开图库");
    }

    pub fn invalidate_similarity(&self) {
        *self.inner.results.lock() = None;
        *self.inner.visuals.write() = None;
    }

    pub fn status(&self, settings: &AiSettings) -> AiStatus {
        let client = AiClient::new(&settings.base_url);
        let health = client.health().ok();
        let process_state = {
            let mut process = self.inner.process.lock();
            if health.is_some() {
                let owned = process.is_owned_running();
                process.mark_online(owned);
            } else {
                process.poll_child();
                if process.state() == ProcessState::External {
                    process.mark_stopped(Some("AI 服务未响应".into()));
                }
            }
            process.state()
        };

        if health.is_none()
            && settings.auto_start
            && !self.inner.manual_stop.load(Ordering::SeqCst)
            && settings.launch_configured()
            && process_state == ProcessState::Stopped
        {
            self.spawn_supervisor(settings.clone());
        }

        AiStatus {
            base_url: settings.base_url.clone(),
            auto_start: settings.auto_start,
            launch_configured: settings.launch_configured(),
            online: health.is_some(),
            process: self.inner.process.lock().status(),
            health,
            mean: self.inner.mean.read().status(),
        }
    }

    pub fn start_service(&self, settings: &AiSettings) -> AppResult<()> {
        if settings.python_path().is_none() {
            return Err(AppError::message("请先在设置中配置 Python 路径"));
        }
        if settings.server_script().is_none() {
            return Err(AppError::message("请先在设置中配置 AI 服务脚本"));
        }
        if !settings.base_url_is_local() {
            return Err(AppError::message(
                "服务地址不是本机地址，只连接外部服务，不启动 Python",
            ));
        }
        self.inner.manual_stop.store(false, Ordering::SeqCst);
        self.inner.process.lock().reset_failure();
        self.inner
            .supervisor_generation
            .fetch_add(1, Ordering::SeqCst);
        self.inner.supervising.store(false, Ordering::SeqCst);
        self.spawn_supervisor(settings.clone());
        Ok(())
    }

    pub fn stop_service(&self, settings: &AiSettings) {
        self.inner.manual_stop.store(true, Ordering::SeqCst);
        self.inner
            .supervisor_generation
            .fetch_add(1, Ordering::SeqCst);
        let owned = self.inner.process.lock().is_owned_running();
        if owned {
            let client = AiClient::new(&settings.base_url);
            let _ = client.shutdown();
            self.inner.process.lock().stop(true);
        } else {
            self.inner.process.lock().mark_stopped(None);
        }
    }

    pub fn shutdown(&self, settings: &AiSettings) {
        self.inner
            .supervisor_generation
            .fetch_add(1, Ordering::SeqCst);
        let owned = self.inner.process.lock().is_owned_running();
        if owned {
            let client = AiClient::new(&settings.base_url);
            let _ = client.shutdown();
            self.inner.process.lock().stop(true);
        }
    }

    /// 设置页保存后按新配置重启托管的服务；外部服务不动。
    pub fn apply_settings(&self, previous: &AiSettings, current: &AiSettings) {
        if previous == current {
            return;
        }
        self.inner
            .supervisor_generation
            .fetch_add(1, Ordering::SeqCst);
        let owned = self.inner.process.lock().is_owned_running();
        if owned {
            let client = AiClient::new(&previous.base_url);
            let _ = client.shutdown();
            self.inner.process.lock().stop(true);
        } else {
            self.inner.process.lock().mark_stopped(None);
        }
        self.inner.manual_stop.store(false, Ordering::SeqCst);
        if current.auto_start && current.launch_configured() {
            self.spawn_supervisor(current.clone());
        }
    }

    pub fn describe_image(
        &self,
        settings: &AiSettings,
        image_id: i64,
        prompt: Option<&str>,
        max_tokens: Option<u32>,
    ) -> AppResult<AiDescriptionResult> {
        let handles = self.handles()?;
        let client = AiClient::new(&settings.base_url);
        let health = client
            .health()
            .map_err(|error| AppError::message(format!("AI 服务不可用：{error}")))?;
        job::ensure_capability(&client, &health, "description")
            .map_err(|error| AppError::message(format!("无法加载描述模型：{error}")))?;
        let bytes = super::input::description_bytes(&handles.database, &handles.root, image_id)?;
        let result = client
            .describe(
                prompt.unwrap_or(""),
                Some(max_tokens.unwrap_or(job::DEFAULT_DESCRIBE_TOKENS)),
                &bytes,
            )
            .map_err(|error| AppError::message(error.to_string()))?;
        if result.text.trim().is_empty() {
            return Err(AppError::message("描述服务返回了空文本"));
        }
        let changed = crate::library::metadata::set_ai_description(
            &handles.database,
            image_id,
            &result.text,
            &result.model,
        )?;
        if !changed {
            return Err(AppError::message("这张图片已有手写描述，未覆盖"));
        }
        Ok(AiDescriptionResult {
            image_id,
            model: result.model,
            text: result.text,
        })
    }

    pub fn generate_pose(&self, settings: &AiSettings, image_id: i64) -> AppResult<PoseData> {
        let handles = self.handles()?;
        let client = AiClient::new(&settings.base_url);
        let health = client
            .health()
            .map_err(|error| AppError::message(format!("AI 服务不可用：{error}")))?;
        let capability = job::ensure_capability(&client, &health, "pose")
            .map_err(|error| AppError::message(format!("无法加载姿势模型：{error}")))?;
        let bytes =
            super::input::small_bytes(&handles.database, &handles.thumbs, &handles.root, image_id)?;
        let result = client
            .pose(&capability.model, &bytes)
            .map_err(|error| AppError::message(error.to_string()))?;
        pose::store(&handles.database, &self.inner.store, image_id, &result)?;
        // 回读一次，确认包头、指针与数据库记录一致后再交给查看器。
        pose::load(&handles.database, &self.inner.store, image_id)?
            .ok_or_else(|| AppError::message("姿势保存后无法读取"))
    }

    pub fn read_pose(&self, image_id: i64) -> AppResult<Option<PoseData>> {
        let handles = self.handles()?;
        pose::load(&handles.database, &self.inner.store, image_id)
    }

    pub fn start_job(
        &self,
        app: AppHandle,
        bus: &TaskBus,
        settings: &AiSettings,
        request: AiJobRequest,
        scope: TaskScope,
        policy: ConflictPolicy,
    ) -> AppResult<TaskStartResult> {
        let handles = self.handles()?;
        // 范围在 Rust 侧解析：全库几十万张也不会走 IPC。
        let request = AiJobRequest {
            image_ids: scope.resolve(&handles.database)?,
            ..request
        }
        .normalized()?;
        let context = JobContext {
            database: handles.database.clone(),
            thumbs: handles.thumbs.clone(),
            root: handles.root.clone(),
            store: self.inner.store.clone(),
            settings: settings.clone(),
        };
        let writes_history = request.kind == AiJobKind::Description;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            request.kind.task_kind(),
            scope.label(),
        );
        let worker_handle = handle.clone();
        let runtime = self.clone();
        let bus_worker = bus.clone();
        let state_app = app.clone();
        bus.launch(app, handle, policy, move |guard| {
            let name = format!("ai-job-{}", worker_handle.id());
            let task = worker_handle.clone();
            let _ = bus_worker.spawn_worker(name, worker_handle, move || {
                // AI lane 由任务线程持有，直到批量执行结束。
                let _guard: LaneGuard = guard;
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    job::execute(&context, &task, &request)
                }));
                let result = match outcome {
                    Ok(result) => result,
                    Err(_) => Err(AppError::message("AI 任务异常终止")),
                };
                task.finish_from(&result);
                // 只有描述任务会产生操作记录：收尾后按最大步数裁剪并安排释放。
                if writes_history && let Some(state) = state_app.try_state::<AppState>() {
                    state.after_history_write();
                }
                *runtime.inner.results.lock() = None;
            });
        })
    }

    /// 语义搜索：text embedding → 与同 model/dim 的图片向量做中心化余弦扫描。
    /// 结果 id 缓存在内存里，按图库代次与 library_id 绑定；关库、换库或数据变化后失效。
    pub fn semantic_search(
        &self,
        settings: &AiSettings,
        text: &str,
        limit: Option<usize>,
    ) -> AppResult<SemanticSearchResult> {
        let text = text.trim();
        if text.is_empty() {
            return Err(AppError::message("请输入语义搜索文本"));
        }
        let handles = self.handles()?;
        let started = Instant::now();
        let client = AiClient::new(&settings.base_url);
        let health = client
            .health()
            .map_err(|error| AppError::message(format!("AI 服务不可用：{error}")))?;
        // 语义搜索只需要文本塔；图片模型名与维度直接从 health 读取，
        // 避免为了拿维度而把图片塔加载进显存。
        let image_capability = health
            .capability("imageEmbedding")
            .ok_or_else(|| AppError::message("AI 服务没有图像 embedding 能力"))?;
        if image_capability.model.trim().is_empty() {
            return Err(AppError::message("embedding 模型名称为空"));
        }
        let image_model = image_capability.model.clone();
        let dim = image_capability
            .dim
            .ok_or_else(|| AppError::message("embedding 模型没有声明维度"))?;
        let text_capability = job::ensure_capability(&client, &health, "textEmbedding")
            .map_err(|error| AppError::message(format!("无法加载文本 embedding 模型：{error}")))?;
        if text_capability.model.trim().is_empty() {
            return Err(AppError::message("文本 embedding 模型名称为空"));
        }
        if text_capability.model != image_model {
            return Err(AppError::message(format!(
                "文本 embedding 模型 {} 与图片 embedding 模型 {image_model} 不一致",
                text_capability.model
            )));
        }
        if let Some(text_dim) = text_capability.dim
            && text_dim != dim
        {
            return Err(AppError::message(format!(
                "文本 embedding 维度 {text_dim} 与图片 embedding 维度 {dim} 不一致"
            )));
        }
        let query_embedding = client
            .embed_text(&text_capability.model, text)
            .map_err(|error| AppError::message(error.to_string()))?;
        if query_embedding.dim != dim {
            return Err(AppError::message(format!(
                "文本 embedding 维度 {} 与图片 embedding 维度 {dim} 不一致",
                query_embedding.dim
            )));
        }
        let mean = self.ensure_mean(&handles, &image_model, dim)?;
        let limit = limit.unwrap_or(embedding::DEFAULT_SEARCH_LIMIT);
        let results = embedding::search(
            &handles.database,
            &self.inner.store,
            &image_model,
            dim,
            &query_embedding.vector,
            &mean.vector,
            limit,
        )?;
        let token = uuid::Uuid::new_v4().to_string();
        let ids: Vec<i64> = results.into_iter().map(|(id, _)| id).collect();
        *self.inner.results.lock() = Some(ResultCache {
            token: token.clone(),
            generation: handles.generation,
            library_id: handles.library_id.clone(),
            ids: ids.clone(),
        });
        Ok(SemanticSearchResult {
            token,
            model: image_model,
            dim,
            total: ids.len(),
            elapsed_ms: started.elapsed().as_millis() as i64,
            mean_count: mean.count,
        })
    }

    /// 目标图相似：`phash`、`color4`、`color8`、`embedding`、`pose`。
    ///
    /// 结果写入统一的 token 缓存，后续分页与全选复用 `get_similarity_page` / `get_similarity_ids`。
    pub fn find_similar_images(
        &self,
        kind: &str,
        target_id: i64,
        threshold: Option<u32>,
        limit: Option<usize>,
    ) -> AppResult<ImageSimilarityResult> {
        let handles = self.handles()?;
        let started = Instant::now();
        let limit = limit
            .unwrap_or(library_similarity::DEFAULT_LIMIT)
            .clamp(1, library_similarity::MAX_LIMIT);
        let ids: Vec<i64> = match kind {
            "phash" => {
                let target = library_similarity::target_visuals(&handles.database, target_id)?;
                let threshold = threshold.unwrap_or(library_similarity::DEFAULT_PHASH_THRESHOLD);
                if threshold > library_similarity::MAX_PHASH_THRESHOLD {
                    return Err(AppError::message("pHash 阈值超出 0-256"));
                }
                let index = self.visual_index(&handles)?;
                index
                    .similar_phash(
                        crate::imageops::phash::words(&target.phash),
                        threshold,
                        limit,
                        target_id,
                    )
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect()
            }
            "color4" | "color8" => {
                let target = library_similarity::target_visuals(&handles.database, target_id)?;
                let index = self.visual_index(&handles)?;
                let (palette, prepared) = if kind == "color4" {
                    (
                        library_similarity::ColorPalette::Colors4,
                        crate::imageops::colors::prepare_palette::<4>(&target.colors4).to_vec(),
                    )
                } else {
                    (
                        library_similarity::ColorPalette::Colors8,
                        crate::imageops::colors::prepare_palette::<8>(&target.colors8).to_vec(),
                    )
                };
                index
                    .similar_color(&prepared, palette, limit, target_id)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect()
            }
            "embedding" => self.similar_ids_by_embedding(&handles, target_id, limit)?,
            "pose" => {
                // pose 记录没有自己的可见性标记，只确认目标图未被移入回收站。
                library_similarity::ensure_visible(&handles.database, target_id)?;
                pose::similar(&handles.database, &self.inner.store, target_id, limit)?
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect()
            }
            other => {
                return Err(AppError::message(format!("未知的相似度类型：{other}")));
            }
        };
        let total = ids.len();
        let token = uuid::Uuid::new_v4().to_string();
        *self.inner.results.lock() = Some(ResultCache {
            token: token.clone(),
            generation: handles.generation,
            library_id: handles.library_id.clone(),
            ids,
        });
        Ok(ImageSimilarityResult {
            token,
            kind: kind.to_owned(),
            total,
            elapsed_ms: started.elapsed().as_millis() as i64,
        })
    }

    fn visual_index(
        &self,
        handles: &LibraryHandles,
    ) -> AppResult<Arc<library_similarity::VisualIndex>> {
        if let Some(index) = self.inner.visuals.read().as_ref()
            && index.is_current(&handles.database)?
        {
            return Ok(index.clone());
        }
        let index = Arc::new(library_similarity::VisualIndex::load(&handles.database)?);
        *self.inner.visuals.write() = Some(index.clone());
        Ok(index)
    }

    fn similar_ids_by_embedding(
        &self,
        handles: &LibraryHandles,
        target_id: i64,
        limit: usize,
    ) -> AppResult<Vec<i64>> {
        let target = embedding::target(&handles.database, &self.inner.store, target_id)?
            .ok_or_else(|| AppError::message("这张图片还没有 embedding，请先运行 AI 任务"))?;
        let mean = self.ensure_mean(handles, &target.model, target.dim)?;
        Ok(embedding::similar_to(
            &handles.database,
            &self.inner.store,
            &target.model,
            target.dim,
            &target.vector,
            &mean.vector,
            target_id,
            limit,
        )?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
    }

    /// 全库相似分组：两种指标可以单独启用，也可以同时启用（同时满足才归组）。
    fn find_similar_groups_with(
        &self,
        options: GroupOptions,
        progress: &dyn GroupProgress,
    ) -> AppResult<SimilarityGroupsResult> {
        let GroupOptions {
            phash: phash_enabled,
            phash_threshold,
            embedding: embedding_enabled,
            embedding_threshold,
        } = options;
        if !phash_enabled && !embedding_enabled {
            return Err(AppError::message("至少选择一种比较方式"));
        }
        let handles = self.handles()?;
        let started = Instant::now();
        let (groups, truncated, missing) = if embedding_enabled {
            let model_rows = embedding::distinct_groups(&handles.database)?;
            let Some((model, dim, _)) = model_rows.into_iter().next() else {
                return Err(AppError::message("还没有图片 embedding，请先运行 AI 任务"));
            };
            let mean = self.ensure_mean(&handles, &model, dim)?;
            // 只开语义时，pHash 仅用来产生候选，不参与筛选；同时开启时按用户阈值收窄候选。
            let candidate_threshold = if phash_enabled {
                phash_threshold.unwrap_or(library_similarity::DEFAULT_PHASH_THRESHOLD)
            } else {
                library_similarity::MAX_INDEXED_PHASH_THRESHOLD
            };
            progress.stage("正在建立候选分组", 0, 700);
            let index = self.visual_index(&handles)?;
            let candidates = index.phash_groups(candidate_threshold, Some(progress))?;
            progress.stage("正在按语义复核", 700, 300);
            let result = embedding::refine_groups(
                &handles.database,
                &self.inner.store,
                &model,
                dim,
                &mean.vector,
                embedding_threshold.unwrap_or(0.97).clamp(0.5, 1.0),
                candidates.groups,
                candidates.truncated,
                Some(progress),
            )?;
            (result.groups, result.truncated, result.missing)
        } else {
            progress.stage("正在建立候选分组", 0, 1000);
            let index = self.visual_index(&handles)?;
            let result = index.phash_groups(
                phash_threshold.unwrap_or(library_similarity::DEFAULT_PHASH_THRESHOLD),
                Some(progress),
            )?;
            (result.groups, result.truncated, 0)
        };
        Ok(SimilarityGroupsResult {
            phash: phash_enabled,
            embedding: embedding_enabled,
            groups,
            truncated,
            missing,
            elapsed_ms: started.elapsed().as_millis() as i64,
        })
    }

    /// 相似分组扫描走任务总线：进度按阶段推进，取消在当前分段/分组结束后生效。
    pub fn start_groups_task(
        &self,
        app: AppHandle,
        bus: &TaskBus,
        options: GroupOptions,
        policy: ConflictPolicy,
    ) -> AppResult<TaskStartResult> {
        if !options.phash && !options.embedding {
            return Err(AppError::message("至少选择一种比较方式"));
        }
        // 先确认图库可用；真正的扫描在线程里跑。
        let _ = self.handles()?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::SimilarityGroups,
            "整个图库",
        );
        let worker = handle.clone();
        let runtime = self.clone();
        let bus_worker = bus.clone();
        bus.launch(app, handle, policy, move |guard| {
            let _ = bus_worker.spawn_worker(
                "similarity-groups".to_owned(),
                worker.clone(),
                move || {
                    let _guard: LaneGuard = guard;
                    worker.set_phase(TaskPhase::Running);
                    worker.set_total(1000);
                    let progress = TaskProgress::new(worker.clone());
                    match runtime.find_similar_groups_with(options, &progress) {
                        Ok(result) => {
                            runtime.store_groups(worker.id(), result);
                            worker.set_message("");
                            worker.set_completed(1000);
                            worker.finish_from(&Ok(()));
                        }
                        Err(error) => worker.finish_from::<()>(&Err(error)),
                    }
                },
            );
        })
    }

    fn store_groups(&self, task_id: &str, result: SimilarityGroupsResult) {
        let mut results = self.inner.groups_results.lock();
        if results.len() >= 5 {
            results.clear();
        }
        results.insert(task_id.to_owned(), result);
    }

    pub fn groups_result(&self, task_id: &str) -> Option<SimilarityGroupsResult> {
        self.inner.groups_results.lock().get(task_id).cloned()
    }

    /// 按 token 分页回填完整行；token 不属于当前图库代次时要求重新搜索。
    pub fn similarity_page(
        &self,
        token: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> AppResult<SimilarityPage> {
        let handles = self.handles()?;
        // 先拷出 id 再查库：查库期间不要占着结果缓存锁，否则并发搜索会被这一页拖住。
        let ids = self.cached_ids(token, &handles)?;
        let limit = limit.unwrap_or(150).clamp(1, 300);
        let start = offset.min(ids.len());
        let end = (start + limit).min(ids.len());
        let items = query::rows_by_ids(&handles.database, &handles.root, &ids[start..end])?;
        Ok(SimilarityPage {
            items,
            next_offset: (end < ids.len()).then_some(end),
            total: ids.len(),
        })
    }

    pub fn similarity_ids(&self, token: &str) -> AppResult<Vec<i64>> {
        let handles = self.handles()?;
        self.cached_ids(token, &handles)
    }

    fn cached_ids(&self, token: &str, handles: &LibraryHandles) -> AppResult<Vec<i64>> {
        let cache = self.inner.results.lock();
        cache
            .as_ref()
            .filter(|cache| {
                cache.token == token
                    && cache.generation == handles.generation
                    && cache.library_id == handles.library_id
            })
            .map(|cache| cache.ids.clone())
            .ok_or_else(|| AppError::message("相似结果已失效，请重新搜索"))
    }

    fn handles(&self) -> AppResult<LibraryHandles> {
        let guard = self.inner.library.read();
        let handles = guard
            .as_ref()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        if handles.generation != self.inner.generation.load(Ordering::SeqCst) {
            return Err(AppError::message("图库已切换"));
        }
        Ok(LibraryHandles {
            generation: handles.generation,
            library_id: handles.library_id.clone(),
            database: handles.database.clone(),
            thumbs: handles.thumbs.clone(),
            root: handles.root.clone(),
        })
    }

    fn ensure_mean(
        &self,
        handles: &LibraryHandles,
        model: &str,
        dim: u32,
    ) -> AppResult<embedding::MeanEmbedding> {
        {
            let slot = self.inner.mean.read();
            if slot.phase == MeanPhase::Ready
                && slot.model.as_deref() == Some(model)
                && slot.dim == Some(dim)
                && let Some(vector) = &slot.vector
            {
                return Ok(embedding::MeanEmbedding {
                    vector: vector.as_ref().clone(),
                    count: slot.count,
                });
            }
        }
        *self.inner.mean.write() = MeanSlot::computing(Some(model.to_owned()), Some(dim));
        let result = embedding::mean(&handles.database, &self.inner.store, model, dim);
        if handles.generation != self.inner.generation.load(Ordering::SeqCst) {
            return Err(AppError::message("图库已切换，请重新搜索"));
        }
        let mut slot = self.inner.mean.write();
        match result {
            Ok(mean) if mean.count > 0 => {
                *slot = MeanSlot::ready(model, dim, &mean);
                Ok(mean)
            }
            Ok(_) => {
                *slot = MeanSlot::empty(
                    Some(model.to_owned()),
                    Some(dim),
                    "还没有图片 embedding，请先运行 AI 任务",
                );
                Err(AppError::message(
                    "还没有图片 embedding，请先运行 AI 任务生成",
                ))
            }
            Err(error) => {
                *slot = MeanSlot::failed(model, dim, error.to_string());
                Err(error)
            }
        }
    }

    fn spawn_mean(&self, generation: u64, database: Arc<LibraryDb>) {
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let groups = match embedding::distinct_groups(&database) {
                Ok(groups) => groups,
                Err(error) => {
                    if generation == inner.generation.load(Ordering::SeqCst) {
                        let mut slot = inner.mean.write();
                        slot.phase = MeanPhase::Failed;
                        slot.message = Some(error.to_string());
                    }
                    return;
                }
            };
            let Some((model, dim, _)) = groups.first().cloned() else {
                if generation == inner.generation.load(Ordering::SeqCst) {
                    *inner.mean.write() = MeanSlot::missing("还没有图片 embedding");
                }
                return;
            };
            if generation != inner.generation.load(Ordering::SeqCst) {
                return;
            }
            *inner.mean.write() = MeanSlot::computing(Some(model.clone()), Some(dim));
            let result = embedding::mean(&database, &inner.store, &model, dim);
            if generation != inner.generation.load(Ordering::SeqCst) {
                return;
            }
            let mut slot = inner.mean.write();
            match result {
                Ok(mean) if mean.count > 0 => *slot = MeanSlot::ready(&model, dim, &mean),
                Ok(_) => {
                    *slot = MeanSlot::empty(Some(model.clone()), Some(dim), "还没有图片 embedding")
                }
                Err(error) => *slot = MeanSlot::failed(&model, dim, error.to_string()),
            }
        });
    }

    fn spawn_supervisor(&self, settings: AiSettings) {
        if self.inner.supervising.swap(true, Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.clone();
        let token = inner.supervisor_generation.load(Ordering::SeqCst);
        std::thread::spawn(move || {
            // 退出时只释放自己这一代，避免把 start_service 新拉起的监督线程标记清掉。
            let _guard = SupervisorGuard {
                inner: inner.clone(),
                token,
            };
            let stale = || token != inner.supervisor_generation.load(Ordering::SeqCst);
            let client = AiClient::new(&settings.base_url);
            if client.health().is_ok() {
                inner.process.lock().mark_online(false);
                return;
            }
            let spec = match launch_spec(&settings) {
                Ok(spec) => spec,
                Err(error) => {
                    inner.process.lock().mark_failed(error.to_string());
                    return;
                }
            };

            let mut online = false;
            for attempt in 1..=MAX_START_ATTEMPTS {
                if stale() {
                    return;
                }
                {
                    let mut process = inner.process.lock();
                    process.note_attempt(attempt);
                    if let Err(error) = process.spawn(&spec) {
                        process.mark_failed(error.to_string());
                        break;
                    }
                }
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    if stale() {
                        return;
                    }
                    if client.health().is_ok() {
                        inner.process.lock().mark_online(true);
                        online = true;
                        break;
                    }
                    if !inner.process.lock().poll_child() {
                        break;
                    }
                    if Instant::now() >= deadline {
                        inner.process.lock().stop(false);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                if online {
                    break;
                }
                if attempt < MAX_START_ATTEMPTS {
                    std::thread::sleep(Duration::from_secs(1 << attempt));
                }
            }
            if !online && !stale() {
                let mut process = inner.process.lock();
                if process.state() != ProcessState::Failed {
                    process.mark_failed("AI 服务启动失败");
                }
            }
        });
    }
}

/// 把“阶段内进度”映射到任务条上的 0-1000。
struct TaskProgress {
    handle: Arc<TaskHandle>,
    base: AtomicUsize,
    span: AtomicUsize,
}

impl TaskProgress {
    fn new(handle: Arc<TaskHandle>) -> Self {
        Self {
            handle,
            base: AtomicUsize::new(0),
            span: AtomicUsize::new(1000),
        }
    }
}

impl GroupProgress for TaskProgress {
    fn report(&self, done: usize, total: usize) {
        let base = self.base.load(Ordering::Relaxed);
        let span = self.span.load(Ordering::Relaxed);
        let scaled = done.min(total).saturating_mul(span);
        let value = base + scaled.checked_div(total).unwrap_or(0);
        self.handle.set_completed(value.min(1000));
    }

    fn cancelled(&self) -> bool {
        self.handle.should_cancel()
    }

    fn stage(&self, label: &str, base: usize, span: usize) {
        self.base.store(base, Ordering::Relaxed);
        self.span.store(span, Ordering::Relaxed);
        self.handle.set_message(label);
        self.handle.set_completed(base.min(1000));
    }
}

/// 监督线程的存活标记；Drop 时按代次释放，不覆盖新一代线程。
struct SupervisorGuard {
    inner: Arc<Inner>,
    token: u64,
}

impl Drop for SupervisorGuard {
    fn drop(&mut self) {
        if self.token == self.inner.supervisor_generation.load(Ordering::SeqCst) {
            self.inner.supervising.store(false, Ordering::SeqCst);
        }
    }
}

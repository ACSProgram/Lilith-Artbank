use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::{
    app::{
        error::{AppError, AppResult},
        settings::AiSettings,
    },
    library::{
        db::LibraryDb,
        paths::LibraryPaths,
        query::{self, ImageRow},
        thumbs::ThumbPack,
        viewer::PoseData,
    },
};

use super::{
    client::{AiClient, Health},
    embedding,
    job::{self, AiJob, AiJobRequest, AiJobSnapshot, JobContext},
    pack::FeatureStore,
    pose,
    process::{AiProcess, MAX_START_ATTEMPTS, ProcessState, ProcessStatus},
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
    pub executable_configured: bool,
    pub online: bool,
    pub process: ProcessStatus,
    pub health: Option<Health>,
    pub mean: MeanStatus,
    pub job: Option<AiJobSnapshot>,
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

struct SemanticCache {
    token: String,
    generation: u64,
    library_id: String,
    ids: Vec<i64>,
}

struct Inner {
    process: Mutex<AiProcess>,
    mean: RwLock<MeanSlot>,
    semantic: Mutex<Option<SemanticCache>>,
    job: Mutex<Option<Arc<AiJob>>>,
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
                semantic: Mutex::new(None),
                job: Mutex::new(None),
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
        *self.inner.semantic.lock() = None;
        *self.inner.mean.write() = MeanSlot::computing(None, None);
        self.spawn_mean(generation, database);
    }

    pub fn library_closed(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        *self.inner.library.write() = None;
        self.inner.store.clear();
        *self.inner.semantic.lock() = None;
        *self.inner.mean.write() = MeanSlot::missing("尚未打开图库");
        if let Some(job) = self.inner.job.lock().as_ref() {
            job.request_cancel();
        }
    }

    pub fn invalidate_semantic(&self) {
        *self.inner.semantic.lock() = None;
    }

    pub fn job_running(&self) -> bool {
        self.inner
            .job
            .lock()
            .as_ref()
            .is_some_and(|job| job.is_running())
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
            && settings
                .executable_path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
            && process_state == ProcessState::Stopped
        {
            self.spawn_supervisor(settings.clone());
        }

        AiStatus {
            base_url: settings.base_url.clone(),
            auto_start: settings.auto_start,
            executable_configured: settings
                .executable_path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty()),
            online: health.is_some(),
            process: self.inner.process.lock().status(),
            health,
            mean: self.inner.mean.read().status(),
            job: self.current_job(),
        }
    }

    pub fn current_job(&self) -> Option<AiJobSnapshot> {
        self.inner.job.lock().as_ref().map(|job| job.snapshot())
    }

    pub fn start_service(&self, settings: &AiSettings) -> AppResult<()> {
        if settings
            .executable_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .is_none()
        {
            return Err(AppError::message("请先在设置中选择 AI 服务可执行文件"));
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

    pub fn ensure_models(
        &self,
        settings: &AiSettings,
        capabilities: &[String],
    ) -> AppResult<AiStatus> {
        let client = AiClient::new(&settings.base_url);
        client
            .ensure_models(capabilities)
            .map_err(|error| AppError::message(format!("加载模型失败：{error}")))?;
        Ok(self.status(settings))
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
        settings: &AiSettings,
        request: AiJobRequest,
    ) -> AppResult<AiJobSnapshot> {
        let handles = self.handles()?;
        let mut slot = self.inner.job.lock();
        if slot.as_ref().is_some_and(|job| job.is_running()) {
            return Err(AppError::message("已有 AI 任务在运行"));
        }
        let job = Arc::new(AiJob::new(uuid::Uuid::new_v4().to_string(), request.kind));
        *slot = Some(job.clone());
        drop(slot);

        let context = JobContext {
            database: handles.database.clone(),
            thumbs: handles.thumbs.clone(),
            root: handles.root.clone(),
            store: self.inner.store.clone(),
            settings: settings.clone(),
        };
        spawn_job_progress(app.clone(), job.clone());
        let runtime = self.clone();
        let fallback_app = app.clone();
        let worker_job = job.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("ai-job-{}", job.id()))
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    job::execute(&context, &worker_job, &request)
                }));
                let phase = if worker_job.should_cancel() {
                    job::AiJobPhase::Cancelled
                } else {
                    match outcome {
                        Ok(Ok(())) => job::AiJobPhase::Completed,
                        Ok(Err(error)) => {
                            worker_job.set_message(error.to_string());
                            job::AiJobPhase::Failed
                        }
                        Err(_) => {
                            worker_job.set_message("AI 任务异常终止");
                            job::AiJobPhase::Failed
                        }
                    }
                };
                worker_job.finish(phase);
                *runtime.inner.semantic.lock() = None;
                let _ = app.emit("ai-job-progress", worker_job.progress());
                let _ = app.emit("ai-job-finished", worker_job.snapshot());
            });
        if let Err(error) = spawn {
            log::error!("无法启动 AI 任务线程：{error}");
            job.set_message(format!("无法启动 AI 任务：{error}"));
            job.finish(job::AiJobPhase::Failed);
            let _ = fallback_app.emit("ai-job-finished", job.snapshot());
            return Err(AppError::message("无法启动 AI 任务"));
        }
        Ok(job.snapshot())
    }

    pub fn cancel_job(&self, job_id: &str) -> AppResult<()> {
        let slot = self.inner.job.lock();
        let job = slot
            .as_ref()
            .filter(|job| job.id() == job_id)
            .ok_or_else(|| AppError::message("AI 任务不存在或已被替换"))?;
        job.request_cancel();
        Ok(())
    }

    pub fn job_status(&self, job_id: &str) -> Option<AiJobSnapshot> {
        self.inner
            .job
            .lock()
            .as_ref()
            .filter(|job| job.id() == job_id)
            .map(|job| job.snapshot())
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
        let image_capability = job::ensure_capability(&client, &health, "imageEmbedding")
            .map_err(|error| AppError::message(format!("无法加载 embedding 模型：{error}")))?;
        if image_capability.model.trim().is_empty() {
            return Err(AppError::message("embedding 模型名称为空"));
        }
        let dim = image_capability
            .dim
            .ok_or_else(|| AppError::message("embedding 模型没有声明维度"))?;
        let text_capability = job::ensure_capability(&client, &health, "textEmbedding")
            .map_err(|error| AppError::message(format!("无法加载文本 embedding 模型：{error}")))?;
        if let Some(text_dim) = text_capability.dim
            && text_dim != dim
        {
            return Err(AppError::message(format!(
                "文本 embedding 维度 {text_dim} 与图片 embedding 维度 {dim} 不一致"
            )));
        }
        let text_model = if text_capability.model.trim().is_empty() {
            image_capability.model.clone()
        } else {
            text_capability.model.clone()
        };
        let query_embedding = client
            .embed_text(&text_model, text)
            .map_err(|error| AppError::message(error.to_string()))?;
        if query_embedding.dim != dim {
            return Err(AppError::message(format!(
                "文本 embedding 维度 {} 与图片 embedding 维度 {dim} 不一致",
                query_embedding.dim
            )));
        }
        let mean = self.ensure_mean(&handles, &image_capability.model, dim)?;
        let limit = limit.unwrap_or(embedding::DEFAULT_SEARCH_LIMIT);
        let results = embedding::search(
            &handles.database,
            &self.inner.store,
            &image_capability.model,
            dim,
            &query_embedding.vector,
            &mean.vector,
            limit,
        )?;
        let token = uuid::Uuid::new_v4().to_string();
        let ids: Vec<i64> = results.into_iter().map(|(id, _)| id).collect();
        *self.inner.semantic.lock() = Some(SemanticCache {
            token: token.clone(),
            generation: handles.generation,
            library_id: handles.library_id.clone(),
            ids: ids.clone(),
        });
        Ok(SemanticSearchResult {
            token,
            model: image_capability.model,
            dim,
            total: ids.len(),
            elapsed_ms: started.elapsed().as_millis() as i64,
            mean_count: mean.count,
        })
    }

    /// 按 token 分页回填完整行；token 不属于当前图库代次时要求重新搜索。
    pub fn similarity_page(
        &self,
        token: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> AppResult<SimilarityPage> {
        let handles = self.handles()?;
        let cache = self.inner.semantic.lock();
        let cache = cache
            .as_ref()
            .filter(|cache| {
                cache.token == token
                    && cache.generation == handles.generation
                    && cache.library_id == handles.library_id
            })
            .ok_or_else(|| AppError::message("语义搜索结果已失效，请重新搜索"))?;
        let limit = limit.unwrap_or(150).clamp(1, 300);
        let start = offset.min(cache.ids.len());
        let end = (start + limit).min(cache.ids.len());
        let ids = &cache.ids[start..end];
        let items = query::rows_by_ids(&handles.database, &handles.root, ids)?;
        Ok(SimilarityPage {
            items,
            next_offset: (end < cache.ids.len()).then_some(end),
            total: cache.ids.len(),
        })
    }

    pub fn similarity_ids(&self, token: &str) -> AppResult<Vec<i64>> {
        let handles = self.handles()?;
        let cache = self.inner.semantic.lock();
        let cache = cache
            .as_ref()
            .filter(|cache| {
                cache.token == token
                    && cache.generation == handles.generation
                    && cache.library_id == handles.library_id
            })
            .ok_or_else(|| AppError::message("语义搜索结果已失效，请重新搜索"))?;
        Ok(cache.ids.clone())
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
            let Some(executable) = settings
                .executable_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
            else {
                return;
            };

            let mut online = false;
            for attempt in 1..=MAX_START_ATTEMPTS {
                if stale() {
                    return;
                }
                {
                    let mut process = inner.process.lock();
                    process.note_attempt(attempt);
                    if let Err(error) = process.spawn(executable, settings.port) {
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

fn spawn_job_progress(app: AppHandle, job: Arc<AiJob>) {
    std::thread::spawn(move || {
        loop {
            let _ = app.emit("ai-job-progress", job.progress());
            if !job.is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    });
}

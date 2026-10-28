use std::{
    collections::{HashMap, HashSet},
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
    concurrency::{
        budget::ImageBudget,
        resources::{Access, Conflict, HolderInfo, ResourceGuard, ResourceRequest, ResourceTable},
    },
    imageops,
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
    /// 缓存下来、可分页与全选的结果条数（受 limit 截断）。
    pub total: usize,
    /// 截断前的命中总数。
    pub total_hits: usize,
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
    /// 缓存下来、可分页与全选的结果条数（受 limit 截断）。
    pub total: usize,
    /// 截断前的命中总数。
    pub total_hits: usize,
    pub elapsed_ms: i64,
}

fn reference_ids(ids: &[i64]) -> AppResult<Vec<i64>> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if *id <= 0 {
            return Err(AppError::message("参考图 id 无效"));
        }
        if seen.insert(*id) {
            result.push(*id);
        }
    }
    if result.is_empty() {
        return Err(AppError::message("至少需要一张参考图"));
    }
    if result.len() > library_similarity::MAX_REFERENCE_IDS {
        return Err(AppError::message(format!(
            "参考图最多 {} 张",
            library_similarity::MAX_REFERENCE_IDS
        )));
    }
    Ok(result)
}

fn single_reference(ids: &[i64], label: &str) -> AppResult<i64> {
    if ids.len() != 1 {
        return Err(AppError::message(format!("{label}：只支持单张参考图")));
    }
    Ok(ids[0])
}

/// pHash 距离上限；省略时用默认 6，最大 256（等于不限制）。
fn parse_phash_threshold(value: Option<f64>) -> AppResult<u32> {
    let raw = value.unwrap_or(f64::from(library_similarity::DEFAULT_PHASH_THRESHOLD));
    let max = f64::from(library_similarity::MAX_PHASH_THRESHOLD);
    if !raw.is_finite() || !(0.0..=max).contains(&raw) {
        return Err(AppError::message("pHash 阈值必须是 0-256"));
    }
    Ok(raw.round() as u32)
}

fn parse_distance_threshold(value: Option<f64>, label: &str) -> AppResult<Option<f32>> {
    match value {
        None => Ok(None),
        Some(value) if value.is_finite() && (0.0..=1.0).contains(&value) => Ok(Some(value as f32)),
        Some(_) => Err(AppError::message(format!("{label}必须在 0 到 1 之间"))),
    }
}

fn parse_score_threshold(value: Option<f64>, label: &str) -> AppResult<Option<f32>> {
    match value {
        None => Ok(None),
        Some(value) if value.is_finite() && (-1.0..=1.0).contains(&value) => Ok(Some(value as f32)),
        Some(_) => Err(AppError::message(format!("{label}必须在 -1 到 1 之间"))),
    }
}

/// 单张参考直接用自身主色；多张时等权合并成一组代表色。
fn reference_colors<const N: usize>(
    database: &LibraryDb,
    targets: &[i64],
    colors_of: impl for<'a> Fn(&'a library_similarity::TargetPalette) -> &'a [u8],
) -> AppResult<Vec<imageops::colors::LabColor>> {
    if let [target_id] = targets {
        let visuals = library_similarity::target_visuals(database, *target_id)?;
        let palette = library_similarity::TargetPalette {
            colors4: visuals.colors4,
            colors8: visuals.colors8,
        };
        return Ok(imageops::colors::prepare_palette::<N>(colors_of(&palette)).to_vec());
    }
    let palettes = library_similarity::target_palettes(database, targets)?;
    let prepared: Vec<_> = palettes
        .iter()
        .map(|palette| imageops::colors::prepare_palette::<N>(colors_of(palette)))
        .collect();
    Ok(imageops::colors::aggregate_palettes(&prepared).to_vec())
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
    /// 已按相似度排好序的命中；phash/color 是距离（越小越近），其余是相似度（越大越近）。
    hits: Vec<(i64, f32)>,
}

struct Inner {
    process: Mutex<AiProcess>,
    mean: RwLock<MeanSlot>,
    results: Mutex<Option<ResultCache>>,
    visuals: RwLock<Option<Arc<library_similarity::VisualIndex>>>,
    groups_results: Mutex<HashMap<String, SimilarityGroupsResult>>,
    store: Arc<FeatureStore>,
    library: RwLock<Option<LibraryHandles>>,
    resources: Arc<ResourceTable>,
    images: Arc<ImageBudget>,
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
        Self::with_runtime(
            Arc::new(ResourceTable::new()),
            Arc::new(ImageBudget::default()),
        )
    }

    pub fn with_runtime(resources: Arc<ResourceTable>, images: Arc<ImageBudget>) -> Self {
        Self {
            inner: Arc::new(Inner {
                process: Mutex::new(AiProcess::default()),
                mean: RwLock::new(MeanSlot::missing("尚未打开图库")),
                results: Mutex::new(None),
                visuals: RwLock::new(None),
                groups_results: Mutex::new(HashMap::new()),
                store: Arc::new(FeatureStore::new()),
                library: RwLock::new(None),
                resources,
                images,
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

    fn acquire_ai_short(
        &self,
        label: &str,
        metadata: bool,
    ) -> AppResult<(HolderInfo, ResourceGuard)> {
        let holder = HolderInfo::new(uuid::Uuid::new_v4().to_string(), label)
            .with_reason("正在使用 AI 服务")
            .with_path_note("正在读取");
        let request = if metadata {
            ResourceRequest::session_read()
                .with_ai_service()
                .with_content_writer()
                .with_timeline(Access::Read)
                .with_metadata_writer()
        } else {
            ResourceRequest::session_read()
                .with_ai_service()
                .with_content_writer()
        };
        match self.inner.resources.try_acquire(&request, &holder) {
            Ok(guard) => Ok((holder, guard)),
            Err(conflicts) => {
                let detail = conflicts
                    .first()
                    .map(Conflict::sentence)
                    .unwrap_or_else(|| "AI 服务正在被其他任务使用".to_owned());
                Err(AppError::message(format!(
                    "{detail}，请等待完成或取消后再试"
                )))
            }
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
        let (holder, _guard) = self.acquire_ai_short("AI 描述", true)?;
        let client = AiClient::new(&settings.base_url);
        let health = client
            .health()
            .map_err(|error| AppError::message(format!("AI 服务不可用：{error}")))?;
        job::ensure_capability(&client, &health, "description")
            .map_err(|error| AppError::message(format!("无法加载描述模型：{error}")))?;
        let bytes = super::input::description_bytes(
            &handles.database,
            &handles.root,
            image_id,
            &self.inner.resources,
            &self.inner.images,
            &holder,
            &|| false,
        )?;
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
        let (holder, _guard) = self.acquire_ai_short("AI 姿势", false)?;
        let client = AiClient::new(&settings.base_url);
        let health = client
            .health()
            .map_err(|error| AppError::message(format!("AI 服务不可用：{error}")))?;
        let capability = job::ensure_capability(&client, &health, "pose")
            .map_err(|error| AppError::message(format!("无法加载姿势模型：{error}")))?;
        let bytes = super::input::small_bytes(
            &handles.database,
            &handles.thumbs,
            &handles.root,
            image_id,
            &self.inner.resources,
            &self.inner.images,
            &holder,
            &|| false,
        )?;
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
        self.launch_ai_job(app, bus, settings, request, scope.label(), None, policy)
    }

    /// 临时区特征生成：只写入 feature_blobs，不产生操作记录；
    /// 任务占用 AI 服务资源，跑完让语义结果与均值失效。
    pub fn start_temp_features(
        &self,
        app: AppHandle,
        bus: &TaskBus,
        settings: &AiSettings,
        kind: AiJobKind,
        image_ids: Vec<i64>,
        policy: ConflictPolicy,
    ) -> AppResult<TaskStartResult> {
        if !matches!(kind, AiJobKind::Embedding | AiJobKind::Pose) {
            return Err(AppError::message("临时区只支持生成 embedding 或姿势特征"));
        }
        let label = format!("临时区 {} 张 · {}", image_ids.len(), kind.label());
        let request = AiJobRequest {
            kind,
            image_ids,
            overwrite: false,
            prompt: None,
            max_tokens: None,
        }
        .normalized()?;
        self.launch_ai_job(
            app,
            bus,
            settings,
            request,
            label,
            Some(TaskKind::TempFeatures),
            policy,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn launch_ai_job(
        &self,
        app: AppHandle,
        bus: &TaskBus,
        settings: &AiSettings,
        request: AiJobRequest,
        scope_label: String,
        task_kind: Option<TaskKind>,
        policy: ConflictPolicy,
    ) -> AppResult<TaskStartResult> {
        let handles = self.handles()?;
        let context = JobContext {
            database: handles.database.clone(),
            thumbs: handles.thumbs.clone(),
            root: handles.root.clone(),
            store: self.inner.store.clone(),
            settings: settings.clone(),
            resources: self.inner.resources.clone(),
            images: self.inner.images.clone(),
        };
        let writes_history = request.kind == AiJobKind::Description;
        let temp_features = task_kind == Some(TaskKind::TempFeatures);
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            task_kind.unwrap_or_else(|| request.kind.task_kind()),
            scope_label,
        );
        let worker_handle = handle.clone();
        let runtime = self.clone();
        let bus_worker = bus.clone();
        let state_app = app.clone();
        let resource_request = if writes_history {
            ResourceRequest::session_read()
                .with_ai_service()
                .with_content_writer()
                .with_timeline(Access::Read)
                .with_metadata_writer()
                .with_path_note("正在读取")
        } else {
            // 即使 embedding/pose 通常只读缩略图，也要避开内容写者：
            // 编辑/压缩会在任务运行期间替换原图与缩略图，旧缩略图算出的特征不能留到新内容上。
            ResourceRequest::session_read()
                .with_ai_service()
                .with_content_writer()
                .with_path_note("正在读取")
        };
        bus.launch(app, handle, resource_request, policy, move |guard| {
            let name = format!("ai-job-{}", worker_handle.id());
            let task = worker_handle.clone();
            let _ = bus_worker.spawn_worker(name, worker_handle, move || {
                // 资源由任务线程持有，直到批量执行结束。
                let _guard: LaneGuard = guard;
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    job::execute(&context, &task, &request)
                }));
                let result = match outcome {
                    Ok(result) => result,
                    Err(_) => Err(AppError::message("AI 任务异常终止")),
                };
                task.finish_from(&result);
                if temp_features {
                    // 特征集合变了：旧的语义结果与均值都不能再用。
                    runtime.invalidate_similarity();
                    *runtime.inner.mean.write() =
                        MeanSlot::missing("临时区特征已更新，下次搜索会重新计算均值");
                } else {
                    *runtime.inner.results.lock() = None;
                }
                // 只有描述任务会产生操作记录：收尾后按最大步数裁剪并安排释放。
                if writes_history && let Some(state) = state_app.try_state::<AppState>() {
                    state.after_history_write();
                }
            });
        })
    }

    /// 语义搜索：text embedding → 与同 model/dim 的图片向量做中心化余弦扫描。
    /// 结果 id 缓存在内存里，按图库代次与 library_id 绑定；关库、换库或数据变化后失效。
    ///
    /// `scope` 把扫描收窄到当前视图/选择/文件夹/集合；`min_score` 是相似度下限。
    pub fn semantic_search(
        &self,
        settings: &AiSettings,
        text: &str,
        limit: Option<usize>,
        scope: Option<&TaskScope>,
        min_score: Option<f64>,
        stage: Option<&dyn Fn(&str)>,
    ) -> AppResult<SemanticSearchResult> {
        let text = text.trim();
        if text.is_empty() {
            return Err(AppError::message("请输入语义搜索文本"));
        }
        let handles = self.handles()?;
        let started = Instant::now();
        let report = |label: &str| {
            if let Some(stage) = stage {
                stage(label);
            }
        };
        let client = AiClient::new(&settings.base_url);
        let health = client.health().map_err(|error| {
            if error.transport {
                AppError::message("AI 服务未启动或无法连接，请先在应用设置里启动 AI 服务")
            } else {
                AppError::message(format!("AI 服务不可用：{error}"))
            }
        })?;
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
        report("加载文本模型");
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
        report("生成查询向量");
        let query_embedding = client
            .embed_text(&text_capability.model, text)
            .map_err(|error| AppError::message(error.to_string()))?;
        if query_embedding.dim != dim {
            return Err(AppError::message(format!(
                "文本 embedding 维度 {} 与图片 embedding 维度 {dim} 不一致",
                query_embedding.dim
            )));
        }
        report("计算均值");
        let mean = self.ensure_mean(&handles, &image_model, dim)?;
        let limit = limit.unwrap_or(embedding::DEFAULT_SEARCH_LIMIT);
        let allowed = self.resolve_similarity_scope(&handles.database, scope)?;
        let min_score = parse_score_threshold(min_score, "语义相似度下限")?;
        report("扫描向量");
        let (hits, total_hits) = embedding::search_with_total(
            &handles.database,
            &self.inner.store,
            &image_model,
            dim,
            &query_embedding.vector,
            &mean.vector,
            limit,
            embedding::SearchFilter {
                allowed: allowed.as_ref(),
                excluded: None,
                min_score,
            },
        )?;
        let token = uuid::Uuid::new_v4().to_string();
        let returned = hits.len();
        *self.inner.results.lock() = Some(ResultCache {
            token: token.clone(),
            generation: handles.generation,
            library_id: handles.library_id.clone(),
            hits,
        });
        Ok(SemanticSearchResult {
            token,
            model: image_model,
            dim,
            total: returned,
            total_hits,
            elapsed_ms: started.elapsed().as_millis() as i64,
            mean_count: mean.count,
        })
    }

    /// 目标图相似：`phash`、`color4`、`color8`、`embedding`、`pose`。
    ///
    /// 单张参考图支持全部指标；多张参考图只支持主色与 embedding 的平均。
    /// `scope` 把扫描收窄到当前视图/选择/文件夹/集合；`None` 与全库等价。
    /// 结果写入统一的 token 缓存，后续分页与全选复用 `get_similarity_page` / `get_similarity_ids`。
    pub fn find_similar_images(
        &self,
        kind: &str,
        target_ids: &[i64],
        threshold: Option<f64>,
        limit: Option<usize>,
        scope: Option<&TaskScope>,
    ) -> AppResult<ImageSimilarityResult> {
        let handles = self.handles()?;
        let started = Instant::now();
        let limit = limit
            .unwrap_or(library_similarity::DEFAULT_LIMIT)
            .clamp(1, library_similarity::MAX_LIMIT);
        let targets = reference_ids(target_ids)?;
        let exclude: HashSet<i64> = targets.iter().copied().collect();
        let allowed = self.resolve_similarity_scope(&handles.database, scope)?;
        let (hits, total_hits): (Vec<(i64, f32)>, usize) = match kind {
            "phash" => {
                let target_id = single_reference(&targets, "pHash")?;
                let target = library_similarity::target_visuals(&handles.database, target_id)?;
                let threshold = parse_phash_threshold(threshold)?;
                let index = self.visual_index(&handles)?;
                let (hit, total) = index.similar_phash_with_total(
                    crate::imageops::phash::words(&target.phash),
                    threshold,
                    limit,
                    &exclude,
                    allowed.as_ref(),
                );
                (
                    hit.into_iter()
                        .map(|(id, distance)| (id, distance as f32))
                        .collect(),
                    total,
                )
            }
            "color4" | "color8" => {
                let threshold = parse_distance_threshold(threshold, "主色距离上限")?;
                let index = self.visual_index(&handles)?;
                let (palette, colors) = if kind == "color4" {
                    (
                        library_similarity::ColorPalette::Colors4,
                        reference_colors::<4>(&handles.database, &targets, |palette| {
                            &palette.colors4
                        })?,
                    )
                } else {
                    (
                        library_similarity::ColorPalette::Colors8,
                        reference_colors::<8>(&handles.database, &targets, |palette| {
                            &palette.colors8
                        })?,
                    )
                };
                index.similar_color_with_total(
                    &colors,
                    palette,
                    threshold,
                    limit,
                    &exclude,
                    allowed.as_ref(),
                )
            }
            "embedding" => {
                let threshold = parse_score_threshold(threshold, "语义相似度下限")?;
                self.similar_ids_by_embedding(
                    &handles,
                    &targets,
                    &exclude,
                    threshold,
                    limit,
                    allowed.as_ref(),
                )?
            }
            "pose" => {
                let target_id = single_reference(&targets, "姿势")?;
                // pose 记录没有自己的可见性标记，只确认目标图未被移入回收站。
                library_similarity::ensure_visible(&handles.database, target_id)?;
                let threshold = parse_score_threshold(threshold, "姿势相似度下限")?;
                pose::similar_with_total(
                    &handles.database,
                    &self.inner.store,
                    target_id,
                    threshold,
                    allowed.as_ref(),
                    limit,
                )?
            }
            other => {
                return Err(AppError::message(format!("未知的相似度类型：{other}")));
            }
        };
        let returned = hits.len();
        let token = uuid::Uuid::new_v4().to_string();
        *self.inner.results.lock() = Some(ResultCache {
            token: token.clone(),
            generation: handles.generation,
            library_id: handles.library_id.clone(),
            hits,
        });
        Ok(ImageSimilarityResult {
            token,
            kind: kind.to_owned(),
            total: returned,
            total_hits,
            elapsed_ms: started.elapsed().as_millis() as i64,
        })
    }

    fn resolve_similarity_scope(
        &self,
        database: &LibraryDb,
        scope: Option<&TaskScope>,
    ) -> AppResult<Option<HashSet<i64>>> {
        match scope {
            None | Some(TaskScope::Library) => Ok(None),
            Some(scope) => Ok(Some(scope.resolve(database)?.into_iter().collect())),
        }
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
        target_ids: &[i64],
        exclude: &HashSet<i64>,
        threshold: Option<f32>,
        limit: usize,
        allowed: Option<&HashSet<i64>>,
    ) -> AppResult<(Vec<(i64, f32)>, usize)> {
        let target = if target_ids.len() == 1 {
            embedding::target(&handles.database, &self.inner.store, target_ids[0])?
                .ok_or_else(|| AppError::message("这张图片还没有 embedding，请先运行 AI 任务"))?
        } else {
            embedding::target_average(&handles.database, &self.inner.store, target_ids)?
        };
        let mean = self.ensure_mean(handles, &target.model, target.dim)?;
        embedding::similar_to_with_total(
            &handles.database,
            &self.inner.store,
            &target.model,
            target.dim,
            &target.vector,
            &mean.vector,
            exclude,
            limit,
            embedding::SearchFilter {
                allowed,
                excluded: None,
                min_score: threshold,
            },
        )
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
        bus.launch(
            app,
            handle,
            ResourceRequest::session_read(),
            policy,
            move |guard| {
                let _ = bus_worker.spawn_worker(
                    "similarity-groups".to_owned(),
                    worker.clone(),
                    move || {
                        let _guard: LaneGuard = guard;
                        worker.set_phase(TaskPhase::Running);
                        worker.set_total(1000);
                        let _permit = match runtime.inner.images.acquire(&|| worker.should_cancel())
                        {
                            Ok(permit) => permit,
                            Err(_) => {
                                worker.finish(TaskPhase::Cancelled);
                                return;
                            }
                        };
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
            },
        )
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
            .map(|cache| cache.hits.iter().map(|(id, _)| *id).collect())
            .ok_or_else(|| AppError::message("相似结果已失效，请重新搜索"))
    }

    /// 当前相似结果的 id → 分数表；token 失效时返回空表，由调用方决定怎么提示。
    pub fn similarity_scores(&self, token: &str) -> AppResult<std::collections::HashMap<i64, f32>> {
        let handles = self.handles()?;
        let cache = self.inner.results.lock();
        Ok(cache
            .as_ref()
            .filter(|cache| {
                cache.token == token
                    && cache.generation == handles.generation
                    && cache.library_id == handles.library_id
            })
            .map(|cache| cache.hits.iter().copied().collect())
            .unwrap_or_default())
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
        let ready = |slot: &MeanSlot| -> Option<embedding::MeanEmbedding> {
            if slot.phase != MeanPhase::Ready
                || slot.model.as_deref() != Some(model)
                || slot.dim != Some(dim)
            {
                return None;
            }
            slot.vector.as_ref().map(|vector| embedding::MeanEmbedding {
                vector: vector.as_ref().clone(),
                count: slot.count,
            })
        };
        if let Some(mean) = ready(&self.inner.mean.read()) {
            return Ok(mean);
        }
        // 打开图库后后台线程已经在算：等它，不要在这里覆盖槽位再算一遍。
        if self.inner.mean.read().phase == MeanPhase::Computing {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                std::thread::sleep(Duration::from_millis(25));
                let slot = self.inner.mean.read();
                if slot.phase != MeanPhase::Computing {
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            if let Some(mean) = ready(&self.inner.mean.read()) {
                return Ok(mean);
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

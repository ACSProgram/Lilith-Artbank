//! 图库状态：结构体与共享访问器，领域方法按职责分在子模块。

mod history_service;
mod import;
mod library_ops;
mod maintenance;
mod mcp_gateway;
mod session;

#[cfg(test)]
mod tests;

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tauri::AppHandle;

use crate::{
    ai::runtime::AiRuntime,
    app::{
        error::{AppError, AppResult},
        idle::IdleLoop,
        settings::SettingsStore,
        tasks::TaskBus,
    },
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget},
        resources::{
            Access, Conflict, HolderInfo, PathScope, ResourceGuard, ResourceRequest, ResourceTable,
            WaitError,
        },
    },
    import::job::{ImportJob, ImportSnapshot},
    library::{
        db::LibraryDb, history::RecoveryPlan, inspect::InspectionReport,
        optimize_estimate::OptimizeEstimate, paths::normalize_folder, query,
        service::LibrarySession, thumbs::ThumbPack,
    },
    mcp::{approval::ApprovalQueue, runtime::McpRuntime, sheet::SheetCache},
};

/// MCP 同步写入成功后通知前端刷新；载荷包含来源、操作记录 id 与是否需要关闭查看器。
pub const LIBRARY_CHANGED_EVENT: &str = "library-changed";

pub struct AppState {
    pub settings: SettingsStore,
    pub ai: AiRuntime,
    pub tasks: TaskBus,
    pub mcp: McpRuntime,
    app: AppHandle,
    /// 图像解码内存额度；全进程共用，按估算字节排队，避免多个解码路径叠加撑高峰值。
    image_memory: Arc<ImageMemoryBudget>,
    library: Mutex<Option<LibrarySession>>,
    /// 最近发起的导入；允许排队后可能同时存在多个未结束的导入。
    import_jobs: Mutex<VecDeque<Arc<ImportJob>>>,
    /// 已结束的导入回执；MCP 任务列表在导入结束后仍要能读到统计与预览。
    import_receipts: Mutex<VecDeque<ImportSnapshot>>,
    managed: AtomicBool,
    recovery: Mutex<RecoveryPlan>,
    optimize_estimates: Arc<Mutex<VecDeque<(String, OptimizeEstimate)>>>,
    /// 最近几次检查的报告，按任务 id 索引；被挤掉时界面提示重新扫描。
    inspection_reports: Arc<Mutex<VecDeque<(String, InspectionReport)>>>,
    /// 空闲回收线程；开库时启动，关库时停。
    idle_loop: Mutex<Option<IdleLoop>>,
    mcp_sheets: Arc<Mutex<SheetCache>>,
    mcp_write: Arc<Mutex<()>>,
    mcp_approvals: Arc<ApprovalQueue>,
    mcp_tasks: Arc<Mutex<VecDeque<String>>>,
}

/// MCP 能取消的任务只保留最近这些条，足够覆盖一次确认后的等待窗口。
const MAX_MCP_TASKS: usize = 100;
/// 导入回执与未结束导入的句柄保留同样量级，旧的滚动淘汰；运行中的导入永不淘汰。
const MAX_IMPORT_RECEIPTS: usize = 20;
/// 检查报告缓存条数；与压缩估算的 10 条同一个滚动淘汰模式。
const MAX_INSPECTION_REPORTS: usize = 5;

/// 缩略图重建连续失败达到该数量即停止，避免把整库都记成失败。
const MAX_CONSECUTIVE_FAILURES: usize = 5;

impl AppState {
    pub fn new(settings: SettingsStore, app: AppHandle) -> Self {
        let mcp_approvals = Arc::new(ApprovalQueue::new(app.clone()));
        let images = Arc::new(ImageBudget::from_setting(
            settings.snapshot().performance.worker_threads,
        ));
        let image_memory = Arc::new(ImageMemoryBudget::default());
        let tasks = TaskBus::with_images(images.clone());
        let ai = AiRuntime::with_runtime(tasks.resources(), images, image_memory.clone());
        Self {
            settings,
            ai,
            tasks,
            mcp: McpRuntime::new(),
            app,
            image_memory,
            library: Mutex::new(None),
            import_jobs: Mutex::new(VecDeque::new()),
            import_receipts: Mutex::new(VecDeque::new()),
            managed: AtomicBool::new(false),
            recovery: Mutex::new(RecoveryPlan::default()),
            optimize_estimates: Arc::new(Mutex::new(VecDeque::new())),
            inspection_reports: Arc::new(Mutex::new(VecDeque::new())),
            idle_loop: Mutex::new(None),
            mcp_sheets: Arc::new(Mutex::new(SheetCache::default())),
            mcp_write: Arc::new(Mutex::new(())),
            mcp_approvals,
            mcp_tasks: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn current_thumb_source(&self) -> Option<(Arc<LibraryDb>, Arc<ThumbPack>)> {
        self.library_handles()
            .map(|(database, thumbs, _)| (database, thumbs))
    }

    pub fn current_library(&self) -> Option<(Arc<LibraryDb>, PathBuf)> {
        self.library_handles()
            .map(|(database, _, root)| (database, root))
    }

    /// 一次取锁拿全当前图库的三件句柄，避免同一操作里重复加锁。
    fn library_handles(&self) -> Option<(Arc<LibraryDb>, Arc<ThumbPack>, PathBuf)> {
        self.library.lock().as_ref().map(|session| {
            (
                session.db.clone(),
                session.thumbs.clone(),
                PathBuf::from(&session.info.path),
            )
        })
    }

    pub(crate) fn resources(&self) -> Arc<ResourceTable> {
        self.tasks.resources()
    }

    pub(crate) fn images(&self) -> Arc<ImageBudget> {
        self.tasks.images()
    }

    pub(crate) fn memory(&self) -> Arc<ImageMemoryBudget> {
        self.image_memory.clone()
    }

    pub(crate) fn move_task_request(
        &self,
        image_ids: &[i64],
        folder_path: &str,
    ) -> AppResult<ResourceRequest> {
        let (database, _) = self.query_source()?;
        move_resource_request(&database, image_ids, folder_path)
    }

    fn short_holder(label: &str, reason: impl Into<String>) -> HolderInfo {
        HolderInfo::new(uuid::Uuid::new_v4().to_string(), label).with_reason(reason)
    }

    fn wait_error(error: WaitError, action: &str) -> AppError {
        match error {
            WaitError::Cancelled => AppError::message("操作已取消"),
            WaitError::Timeout(conflicts) => AppError::message(format!(
                "{}，请等待完成或取消后再{action}",
                conflict_sentence(&conflicts)
            )),
        }
    }

    /// 文件夹结构改动用：会话读 + 时间线共享 + 路径写，最多等 10 秒。
    pub(crate) fn begin_folder_change(
        &self,
        label: &str,
        paths: Vec<PathScope>,
    ) -> AppResult<ResourceGuard> {
        let holder = Self::short_holder(label, "正在维护文件夹");
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_paths(paths);
        self.resources()
            .acquire(
                &request,
                &holder,
                Some(Instant::now() + Duration::from_secs(10)),
                &|| false,
            )
            .map_err(|error| Self::wait_error(error, "重试"))
    }

    /// 打开/创建/关闭图库：要求没有任务、短操作与排队任务。
    fn begin_session_write(
        &self,
        label: &str,
        action: &str,
        wait: Option<Duration>,
    ) -> AppResult<ResourceGuard> {
        let holder = Self::short_holder(label, "正在切换图库");
        let request = ResourceRequest::session_write();
        let table = self.resources();
        match wait {
            Some(wait) => table
                .acquire(&request, &holder, Some(Instant::now() + wait), &|| false)
                .map_err(|error| Self::wait_error(error, action)),
            None => table.try_acquire(&request, &holder).map_err(|conflicts| {
                AppError::message(format!(
                    "{}，请等待完成或取消后再{action}",
                    conflict_sentence(&conflicts)
                ))
            }),
        }
    }

    pub(crate) fn begin_metadata_write(&self, label: &str) -> AppResult<ResourceGuard> {
        let holder = Self::short_holder(label, "正在修改图片信息");
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_metadata_writer();
        self.tasks
            .acquire_immediate(&request, &holder)
            .map(|guard| guard.into_resource())
    }

    /// 编辑保存等短内容写：会话读 + 时间线共享 + 内容写 + 路径写，立即失败。
    pub(crate) fn begin_content_write(
        &self,
        label: &str,
        paths: Vec<PathScope>,
    ) -> AppResult<ResourceGuard> {
        let holder = Self::short_holder(label, "正在写入图库内容");
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_content_writer()
            .with_paths(paths);
        self.tasks
            .acquire_immediate(&request, &holder)
            .map(|guard| guard.into_resource())
    }

    /// `application.manage(state)` 之后调用：启动时打开的库要等状态注册完再安排恢复任务。
    pub fn after_manage(&self) {
        self.managed.store(true, Ordering::SeqCst);
        self.after_library_open();
        self.mcp
            .apply(self.app.clone(), &self.settings.snapshot().mcp);
    }

    fn query_source(&self) -> AppResult<(Arc<LibraryDb>, PathBuf)> {
        self.library_handles()
            .map(|(database, _, root)| (database, root))
            .ok_or_else(|| AppError::message("尚未打开图库"))
    }

    /// 会改变图片集合、排序字段或筛选字段的写入：成功后让 id 列表游标整体失效。
    ///
    /// 有序 id 列表按查询指纹缓存，任何一条这样的写入都可能让它过期；失败时数据库已回滚，
    /// 缓存继续有效，不必清空。
    fn mutate<T>(&self, operation: impl FnOnce() -> AppResult<T>) -> AppResult<T> {
        let result = operation();
        if result.is_ok() {
            query::clear_page_cache();
            self.after_history_write();
        }
        result
    }
}

fn conflict_sentence(conflicts: &[Conflict]) -> String {
    conflicts
        .first()
        .map(Conflict::sentence)
        .unwrap_or_else(|| "有操作正在进行".to_owned())
}

/// 移动图片的任务级路径写：源文件夹与目标文件夹；源文件夹过多时退化为根写。
fn move_resource_request(
    database: &LibraryDb,
    image_ids: &[i64],
    target: &str,
) -> AppResult<ResourceRequest> {
    let mut sources = std::collections::HashSet::new();
    for chunk in image_ids.chunks(400) {
        let sql = format!(
            "SELECT DISTINCT folders.path FROM images
             JOIN folders ON folders.id = images.folder_id
             WHERE images.id IN ({})",
            crate::library::placeholders(chunk.len())
        );
        database.with_reader(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                row.get::<_, String>(0)
            })?;
            for path in rows {
                sources.insert(path?);
            }
            Ok(())
        })?;
    }
    let mut paths = if sources.is_empty() || sources.len() > 64 {
        vec![PathScope::write("")]
    } else {
        sources.into_iter().map(PathScope::write).collect()
    };
    paths.push(PathScope::write(normalize_folder(target)?));
    Ok(ResourceRequest::session_read()
        .with_timeline(Access::Read)
        .with_content_writer()
        .with_paths(paths)
        .with_path_note("正在移动图片"))
}

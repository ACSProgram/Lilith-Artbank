//! 图库生命周期：创建、打开、关闭、换库、重命名、退出与状态查询。

use std::{
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

use crate::{
    app::{
        activity,
        error::{AppError, AppResult},
        idle::IdleLoop,
        tasks::TaskPhase,
    },
    library::{
        history::{self, RecoveryPlan},
        manifest::{LibraryInspection, inspect},
        query,
        service::{self, LibraryCounts, LibraryInfo},
        temp,
    },
};

use super::AppState;

impl AppState {
    /// 返回缓存的状态；计数不再随每次状态查询重算，由 [`Self::counts`] 按需刷新。
    pub fn status(&self) -> AppResult<Option<LibraryInfo>> {
        Ok(self
            .library
            .lock()
            .as_ref()
            .map(|session| session.info.clone()))
    }

    /// 重算三档计数并写回缓存；前端在开库、数据变化后单独调用。
    pub fn counts(&self) -> AppResult<LibraryCounts> {
        let Some((database, library_id)) = self
            .library
            .lock()
            .as_ref()
            .map(|session| (session.db.clone(), session.info.id.clone()))
        else {
            return Err(AppError::message("尚未打开图库"));
        };
        let (image_count, trash_count, temp_count) = database.counts()?;
        let mut library = self.library.lock();
        if let Some(session) = library.as_mut()
            && session.info.id == library_id
        {
            session.info.image_count = image_count;
            session.info.trash_count = trash_count;
            session.info.temp_count = temp_count;
        }
        Ok(LibraryCounts {
            image_count,
            trash_count,
            temp_count,
        })
    }

    pub fn create_library(&self, path: &Path, name: &str) -> AppResult<LibraryInfo> {
        let _session = self.begin_session_write("创建图库", "创建图库", None)?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let (session, _manifest) = service::create(path, name, read_connections)?;
        let info = session.info.clone();
        self.settings.set_library_path(Some(info.path.clone()))?;
        self.clear_previous_temp();
        self.ai.library_opened(
            info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            PathBuf::from(&info.path),
        );
        // 刚开库算「人在」，别让空闲循环在用户还没碰过界面时先动手。
        activity::touch();
        *self.library.lock() = Some(session);
        *self.recovery.lock() = RecoveryPlan::default();
        self.optimize_estimates.lock().clear();
        self.inspection_reports.lock().clear();
        self.import_receipts.lock().clear();
        self.import_jobs.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        // 换库算换了一代：换库期间提交、还在排队的任务拿到资源后会直接取消。
        self.tasks.bump_epoch();
        self.restart_idle_loop();
        Ok(info)
    }

    pub fn open_library(&self, path: &Path) -> AppResult<LibraryInfo> {
        let _session = self.begin_session_write("打开图库", "打开图库", None)?;
        let read_connections = self.settings.snapshot().performance.reader_connections;
        let session = service::open(path, read_connections)?;
        let plan = history::recover_on_open(&session.db)?;
        if let Err(error) = history::cleanup_undo_dirs(&session.db, Path::new(&session.info.path)) {
            log::warn!("清理撤销暂存目录失败：{error}");
        }
        // 上次退出没能清掉的临时图片在这里补清；计数由前端随后单独拉取。
        if let Err(error) = temp::clear(&session.db, Path::new(&session.info.path)) {
            log::warn!("清空临时文件夹失败：{error}");
        }
        let info = session.info.clone();
        self.settings.set_library_path(Some(info.path.clone()))?;
        self.clear_previous_temp();
        self.ai.library_opened(
            info.id.clone(),
            session.db.clone(),
            session.thumbs.clone(),
            PathBuf::from(&info.path),
        );
        // 刚开库算「人在」，别让空闲循环在用户还没碰过界面时先动手。
        activity::touch();
        *self.library.lock() = Some(session);
        *self.recovery.lock() = plan;
        self.optimize_estimates.lock().clear();
        self.inspection_reports.lock().clear();
        self.import_receipts.lock().clear();
        self.import_jobs.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        // 换库算换了一代：换库期间提交、还在排队的任务拿到资源后会直接取消。
        self.tasks.bump_epoch();
        self.restart_idle_loop();
        drop(_session);
        if self.managed.load(Ordering::SeqCst) {
            self.after_library_open();
        }
        Ok(info)
    }

    /// 开库时起空闲回收线程；换库时先停旧的，避免两个循环同时提交任务。
    fn restart_idle_loop(&self) {
        // 两次取锁不嵌套：旧线程的 Drop 要 join，不能在持锁时等。
        *self.idle_loop.lock() = None;
        if self.library.lock().is_some() {
            let started = IdleLoop::start(self.app.clone());
            *self.idle_loop.lock() = Some(started);
        }
    }

    fn stop_idle_loop(&self) {
        *self.idle_loop.lock() = None;
    }

    /// 切换图库时清空旧库的临时区；失败只记日志，不能挡住换库。
    fn clear_previous_temp(&self) {
        let Some((database, _, root)) = self.library_handles() else {
            return;
        };
        if let Err(error) = temp::clear(&database, &root) {
            log::warn!("切换图库前清空临时文件夹失败：{error}");
        }
        self.ai.invalidate_similarity();
        self.mcp_sheets.lock().clear();
    }

    /// 启动时按设置自动打开上次的图库；失败只记录，不阻塞应用启动。
    pub fn open_configured_library(&self) {
        let Some(path) = self.settings.snapshot().library_path else {
            return;
        };
        if let Err(error) = self.open_library(Path::new(&path)) {
            log::warn!("上次打开的图库无法加载（{path}）：{error}");
        }
    }

    pub fn close_library(&self) -> AppResult<()> {
        // 内容写任务不能中途关库；排队中的写任务也算，不能等关库时再启动。
        // 这一步放在停空闲线程之前：停线程要等它退出（最多 1 秒），期间短任务可能已经结束，
        // 拒绝理由就会时有时无。
        if let Some(task) = self
            .tasks
            .active()
            .into_iter()
            .find(|task| !task.kind.can_cancel_for_close())
        {
            let state = if task.phase == TaskPhase::Queued {
                "正在排队"
            } else {
                "正在进行"
            };
            return Err(AppError::message(format!(
                "「{}」{state}，请等待完成或取消后再关闭图库",
                task.label
            )));
        }
        // 先停空闲回收再取消任务：它可能正停在「判断空闲 → 提交回收任务」之间，
        // 那一步会在关库之后落地，拿着旧库的句柄继续压缩略图包。停线程会 join，
        // 返回时不可能再有新的自动回收。
        self.stop_idle_loop();
        self.tasks.cancel_for_close();
        // 可取消任务与短操作释放资源后再关库；超时会点名占用者。
        let _session =
            self.begin_session_write("关闭图库", "关闭图库", Some(Duration::from_secs(10)))?;
        // 从这一刻起图库算换了代：排队任务拿到资源时会发现自己已经过期，直接取消。
        self.tasks.bump_epoch();
        if let Some((database, _, root)) = self.library_handles()
            && let Err(error) = temp::clear(&database, &root)
        {
            log::warn!("关闭图库前清空临时文件夹失败：{error}");
        }
        self.ai.library_closed();
        activity::reset();
        *self.library.lock() = None;
        *self.recovery.lock() = RecoveryPlan::default();
        self.optimize_estimates.lock().clear();
        self.inspection_reports.lock().clear();
        self.import_receipts.lock().clear();
        self.import_jobs.lock().clear();
        self.mcp_sheets.lock().clear();
        self.mcp_approvals.clear();
        query::clear_page_cache();
        // 关库期间提交的任务已经拿着旧句柄，再递增一次让它们一起作废。
        self.tasks.bump_epoch();
        self.settings.set_library_path(None)?;
        Ok(())
    }

    /// 应用退出：没有写任务时尽量清空临时区；来不及清的下次打开图库会补清。
    pub fn shutdown(&self) {
        if self
            .tasks
            .active()
            .iter()
            .any(|task| !task.kind.can_cancel_for_close())
        {
            log::warn!("退出时仍有写任务，临时文件夹留到下次打开图库时清空");
            return;
        }
        // 排队任务还没占资源，退出前直接取消，避免关闭过程中突然启动。
        self.stop_idle_loop();
        self.tasks.cancel_queued();
        self.tasks.cancel_for_close();
        if !self.tasks.wait_idle(Duration::from_secs(2)) {
            log::warn!("退出时任务未停止，临时文件夹留到下次打开图库时清空");
            return;
        }
        let Ok(_session) = self.begin_session_write("退出", "退出", Some(Duration::from_secs(2)))
        else {
            log::warn!("退出时图库仍被占用，临时文件夹留到下次打开图库时清空");
            return;
        };
        if let Some((database, _, root)) = self.library_handles()
            && let Err(error) = temp::clear(&database, &root)
        {
            log::warn!("退出时清空临时文件夹失败：{error}");
        }
    }

    pub fn rename_current_library(&self, name: &str) -> AppResult<LibraryInfo> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 120 {
            return Err(AppError::message("图库名称必须为 1-120 个字符"));
        }
        let mut guard = self.library.lock();
        let session = guard
            .as_mut()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        service::rename_manifest(Path::new(&session.info.path), trimmed)?;
        session.info.name = trimmed.to_owned();
        Ok(session.info.clone())
    }

    pub fn inspect_library(&self, path: &Path) -> LibraryInspection {
        inspect(path)
    }

    /// 图库可用后的收尾：先续做中断的撤销（撤销不排队，要先拿到写资源），再按上限裁剪。
    pub(super) fn after_library_open(&self) {
        self.schedule_recovery();
        self.after_history_write();
    }
}

//! 操作记录：分页、撤销/重做、清空、重试与待删文件释放。

use std::{path::PathBuf, sync::Arc};

use rusqlite::OptionalExtension;

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::{ConflictPolicy, LaneGuard, TaskHandle, TaskKind, TaskPhase, TaskStartResult},
    },
    concurrency::resources::{Access, PathScope, ResourceRequest},
    library::{
        db::LibraryDb,
        history::{self, HistoryPage},
        query,
        thumbs::ThumbPack,
    },
};

use super::AppState;

impl AppState {
    pub fn history_page(&self) -> AppResult<HistoryPage> {
        let (database, root) = self.query_source()?;
        let entries = history::list(&database)?;
        let (pending_purge_count, pending_purge_bytes) = history::pending_summary(&database)?;
        let storage = history::storage_usage(&database, &root)?;
        let history_task_active = self
            .tasks
            .active()
            .iter()
            .any(|task| matches!(task.kind, TaskKind::Undo | TaskKind::Redo));
        // 有中断标记但撤销/重做任务正在跑，说明是本次运行中的中间态，不算遗留。
        let recovery_needed = !history_task_active && history::has_interrupted_run(&database)?;
        // 撤销与重做要独占时间线、内容写、元数据写与根路径；界面在仍有运行中任务时直接置灰。
        let blocked_reason = if recovery_needed {
            Some("上次撤销或重做异常中断，请重新打开图库恢复".to_owned())
        } else {
            match self.tasks.active().first() {
                Some(task) => Some(format!("「{}」正在进行，完成后才能撤销或重做", task.label)),
                None if entries.is_empty() => Some("还没有操作记录".to_owned()),
                None => None,
            }
        };
        Ok(HistoryPage {
            entries,
            max_steps: self.settings.snapshot().history.max_steps,
            pending_purge_count,
            pending_purge_bytes,
            storage,
            recovery_needed,
            can_run: blocked_reason.is_none(),
            blocked_reason,
        })
    }

    pub fn undo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let (database, _) = self.query_source()?;
        let (status, failed, redo_failed, count) = database.with_reader(|connection| {
            let status: Option<String> = connection
                .query_row(
                    "SELECT status FROM operations WHERE id = ?1",
                    [target_id],
                    |row| row.get(0),
                )
                .optional()?;
            let failed: i64 = connection.query_row(
                "SELECT count(*) FROM operations
                 WHERE id >= ?1 AND status = 'undoFailed'",
                [target_id],
                |row| row.get(0),
            )?;
            let redo_failed: i64 = connection.query_row(
                "SELECT count(*) FROM operations
                 WHERE id >= ?1 AND status IN ('undone', 'redoing') AND error <> ''",
                [target_id],
                |row| row.get(0),
            )?;
            let count: i64 = connection.query_row(
                "SELECT count(*) FROM operations
                 WHERE id >= ?1 AND status IN ('applied', 'undoing')",
                [target_id],
                |row| row.get(0),
            )?;
            Ok((status, failed, redo_failed, count))
        })?;
        match status.as_deref() {
            Some("applied") => {}
            Some("undoFailed") => {
                return Err(AppError::message(
                    "这条记录上次撤销失败，需要先处理失败原因",
                ));
            }
            Some("undone") | Some("redoing") => {
                return Err(AppError::message("这条记录已经撤销，可以重做"));
            }
            Some("undoSkipped") => {
                return Err(AppError::message("这条记录已跳过撤销"));
            }
            Some(_) => return Err(AppError::message("只能撤销已经完成的操作")),
            None => return Err(AppError::message("操作记录不存在")),
        }
        if failed > 0 {
            return Err(AppError::message(
                "之后有一条记录上次撤销失败，不能继续回退；请先重试或跳过这条记录",
            ));
        }
        if redo_failed > 0 {
            return Err(AppError::message(
                "之后有一条记录上次重做失败，不能继续回退；请先重试重做或清空记录",
            ));
        }
        if count <= 0 {
            return Err(AppError::message("没有可撤销的记录"));
        }
        self.start_undo_task(target_id, count as usize)
    }

    pub fn redo_to(&self, target_id: i64) -> AppResult<TaskStartResult> {
        let (database, _) = self.query_source()?;
        let (status, count) = database.with_reader(|connection| {
            let status: Option<String> = connection
                .query_row(
                    "SELECT status FROM operations WHERE id = ?1",
                    [target_id],
                    |row| row.get(0),
                )
                .optional()?;
            let mut statement = connection
                .prepare("SELECT status FROM operations WHERE id <= ?1 ORDER BY id DESC")?;
            let statuses = statement
                .query_map([target_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let count = statuses
                .iter()
                .take_while(|status| *status == "undone" || *status == "redoing")
                .count();
            Ok((status, count))
        })?;
        match status.as_deref() {
            Some("undone") | Some("redoing") => {}
            Some("applied") => {
                return Err(AppError::message("这条记录还没有撤销，不能重做"));
            }
            Some("undoFailed") => {
                return Err(AppError::message(
                    "这条记录上次撤销失败，需要先处理失败原因",
                ));
            }
            Some("undoSkipped") => {
                return Err(AppError::message("这条记录已跳过撤销"));
            }
            Some(_) => return Err(AppError::message("只能重做已经撤销的操作")),
            None => return Err(AppError::message("操作记录不存在")),
        }
        if count == 0 {
            return Err(AppError::message("没有可重做的记录"));
        }
        self.start_redo_task(target_id, count)
    }

    pub fn clear_history(&self) -> AppResult<TaskStartResult> {
        let (database, root) = self.query_source()?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::ClearHistory,
            "清空操作记录",
        );
        // 只删除记录与撤销暂存，是一条短事务；待删文件由「释放待删文件」单独处理。
        handle.set_cancellable(false);
        let app = self.app.clone();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        self.tasks.launch(
            app,
            handle,
            ResourceRequest::session_read()
                .with_timeline(Access::Write)
                .with_content_writer()
                .with_paths(vec![PathScope::write("")])
                .with_path_note("正在清空操作记录"),
            ConflictPolicy::Fail,
            move |guard| {
                let database = database.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker(
                    "clear-operation-history".to_owned(),
                    task.clone(),
                    move || {
                        let _guard: LaneGuard = guard;
                        task.set_phase(TaskPhase::Running);
                        let result = history::clear(&database, &root).map(|_| ());
                        query::clear_page_cache();
                        ai.invalidate_similarity();
                        task.finish_from(&result);
                    },
                );
            },
        )
    }

    pub fn retry_undo(&self, operation_id: i64) -> AppResult<TaskStartResult> {
        let (database, _) = self.query_source()?;
        let status: Option<String> = database.with_reader(|connection| {
            Ok(connection
                .query_row(
                    "SELECT status FROM operations WHERE id = ?1",
                    [operation_id],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        match status.as_deref() {
            Some("undoFailed") => self.start_retry_undo_task(operation_id),
            Some(_) => Err(AppError::message("只有撤销失败的记录可以重试")),
            None => Err(AppError::message("操作记录不存在")),
        }
    }

    pub fn skip_undo(&self, operation_id: i64) -> AppResult<()> {
        // 只改记录状态，不动图片与文件；与撤销/重做、清空记录互斥。
        let _guard = self.begin_metadata_write("跳过撤销失败")?;
        let (database, _) = self.query_source()?;
        history::skip_undo(&database, operation_id)
    }

    /// 独立动作：删除已没有记录引用的待释放文件。
    ///
    /// 自动释放可能刚好在跑，排队等它结束即可；清理没有记录引用的批次可以重复执行。
    pub fn release_pending(&self) -> AppResult<TaskStartResult> {
        self.start_release_task("释放待删文件", ConflictPolicy::Queue)
    }

    /// 历史或保留步数变化后按上限裁剪、安排释放；失败只记日志。
    ///
    /// 触发点：经 `mutate` 的写命令、编辑保存（命令里没有经过 `mutate`）、
    /// 导入收尾线程、保存设置（步数可能调小）。
    pub(crate) fn after_history_write(&self) {
        let Some((database, _, root)) = self.library_handles() else {
            return;
        };
        let max_steps = self.settings.snapshot().history.max_steps;
        match history::trim(&database, max_steps) {
            Ok(evicted) => {
                // 新写入会删除重做分支，被裁掉的编辑/导入记录也要清掉暂存目录。
                if let Err(error) = history::cleanup_undo_dirs(&database, &root) {
                    log::warn!("清理撤销暂存目录失败：{error}");
                }
                if evicted
                    .iter()
                    .any(|operation| operation.kind.releases_files())
                {
                    self.schedule_release();
                }
            }
            Err(error) => log::warn!("裁剪操作记录失败：{error}"),
        }
    }

    pub(super) fn schedule_recovery(&self) {
        let plan = {
            let mut guard = self.recovery.lock();
            std::mem::take(&mut *guard)
        };
        let mut resumed = false;
        if let Some(target_id) = plan.resume_undo {
            let Ok((database, _)) = self.query_source() else {
                return;
            };
            let count = database.with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM operations
                     WHERE id >= ?1 AND status IN ('applied', 'undoing')",
                    [target_id],
                    |row| row.get::<_, i64>(0),
                )?)
            });
            match count {
                Ok(count) if count > 0 => {
                    if let Err(error) = self.start_undo_task(target_id, count as usize) {
                        log::warn!("恢复中断的撤销失败：{error}");
                    } else {
                        resumed = true;
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("读取未完成撤销记录失败：{error}"),
            }
        }
        if !resumed && let Some(target_id) = plan.resume_redo {
            let Ok((database, _)) = self.query_source() else {
                return;
            };
            let count = database.with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT count(*) FROM operations
                     WHERE id <= ?1 AND status = 'redoing'",
                    [target_id],
                    |row| row.get::<_, i64>(0),
                )?)
            });
            match count {
                Ok(count) if count > 0 => {
                    if let Err(error) = self.start_redo_task(target_id, count as usize) {
                        log::warn!("恢复中断的重做失败：{error}");
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("读取未完成重做记录失败：{error}"),
            }
        }
        if plan.release_needed {
            self.schedule_release();
        }
    }

    fn schedule_release(&self) {
        if self
            .tasks
            .active()
            .iter()
            .any(|task| task.kind == TaskKind::ReleaseGarbage)
        {
            return;
        }
        let Some((database, _, root)) = self.library_handles() else {
            return;
        };
        match history::has_orphan_purge(&database, &root) {
            Ok(true) => {
                if let Err(error) = self.start_release_task("释放删除文件", ConflictPolicy::Queue)
                {
                    log::warn!("安排释放任务失败：{error}");
                }
            }
            Ok(false) => {}
            Err(error) => log::warn!("检查待释放文件失败：{error}"),
        }
    }

    fn start_undo_task(&self, target_id: i64, count: usize) -> AppResult<TaskStartResult> {
        self.start_history_task(
            TaskKind::Undo,
            format!("撤销 {count} 条记录"),
            count > 1,
            "undo-history",
            "正在撤销操作",
            move |database, root, thumbs, task| {
                history::run_undo(&database, &root, &thumbs, target_id, task)
            },
        )
    }

    fn start_retry_undo_task(&self, operation_id: i64) -> AppResult<TaskStartResult> {
        self.start_history_task(
            TaskKind::Undo,
            "重试撤销失败记录".to_owned(),
            false,
            "retry-undo-history",
            "正在重试撤销",
            move |database, root, thumbs, task| {
                history::retry_undo(&database, &root, &thumbs, operation_id, task)
            },
        )
    }

    fn start_history_task<F>(
        &self,
        kind: TaskKind,
        label: String,
        cancellable: bool,
        worker_name: &'static str,
        path_note: &'static str,
        run: F,
    ) -> AppResult<TaskStartResult>
    where
        F: FnOnce(Arc<LibraryDb>, PathBuf, Arc<ThumbPack>, Arc<TaskHandle>) -> AppResult<()>
            + Send
            + 'static,
    {
        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;
        let handle = TaskHandle::new(uuid::Uuid::new_v4().to_string(), kind, label);
        handle.set_cancellable(cancellable);
        let app = self.app.clone();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let request = ResourceRequest::session_read()
            .with_timeline(Access::Write)
            .with_content_writer()
            .with_metadata_writer()
            .with_paths(vec![PathScope::write("")])
            .with_path_note(path_note);
        self.tasks
            .launch(app, handle, request, ConflictPolicy::Fail, move |guard| {
                let database = database.clone();
                let thumbs = thumbs.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker(worker_name.to_owned(), task.clone(), move || {
                    let _guard: LaneGuard = guard;
                    task.set_phase(TaskPhase::Running);
                    let result = run(database, root, thumbs, task.clone());
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    task.finish_from(&result);
                });
            })
    }

    fn start_redo_task(&self, target_id: i64, count: usize) -> AppResult<TaskStartResult> {
        self.start_history_task(
            TaskKind::Redo,
            format!("重做 {count} 条记录"),
            count > 1,
            "redo-history",
            "正在重做操作",
            move |database, root, thumbs, task| {
                history::run_redo(&database, &root, &thumbs, target_id, task)
            },
        )
    }

    fn start_release_task(
        &self,
        scope: &str,
        policy: ConflictPolicy,
    ) -> AppResult<TaskStartResult> {
        let (database, root) = self.query_source()?;
        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::ReleaseGarbage,
            scope,
        );
        // 可在块边界停下：没处理的行还带着待释放标记，下次释放接着删，
        // 所以关库不必等几十万文件删完，用户也能自己取消。
        handle.set_cancellable(true);
        let app = self.app.clone();
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let ai = self.ai.clone();
        let request = ResourceRequest::session_read()
            .with_content_writer()
            .with_paths(vec![PathScope::write("")])
            .with_path_note("正在释放删除文件");
        self.tasks
            .launch(app, handle, request, policy, move |guard| {
                let database = database.clone();
                let root = root.clone();
                let task = worker.clone();
                let tasks = tasks.clone();
                let ai = ai.clone();
                let _ = tasks.spawn_worker("release-garbage".to_owned(), task.clone(), move || {
                    let _guard: LaneGuard = guard;
                    task.set_phase(TaskPhase::Running);
                    let result = history::run_release(&database, &root, task.clone());
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    finish_release(&task, &result);
                });
            })
    }
}

/// 释放任务的终态：有释放失败就是「部分失败」，让用户知道有些文件还在磁盘上。
fn finish_release(task: &TaskHandle, result: &AppResult<()>) {
    if let Err(error) = result {
        task.set_message(error.to_string());
        task.finish(TaskPhase::Failed);
        return;
    }
    if task.should_cancel() {
        task.finish(TaskPhase::Cancelled);
        return;
    }
    if task.failed() > 0 {
        task.finish(TaskPhase::PartialFailed);
    } else {
        task.finish(TaskPhase::Completed);
    }
}

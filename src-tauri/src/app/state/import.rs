//! 导入：界面与 MCP 共用同一条管线，导入回执、取消与状态查询。

use std::sync::Arc;

use tauri::AppHandle;

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::{ConflictPolicy, TaskStartResult, TaskSummary},
    },
    concurrency::resources::{Access, HolderInfo, PathScope, ResourceRequest},
    import::{
        ImportRequest, ImportSource, ImportTarget,
        job::{ImportJob, ImportSnapshot},
    },
    library::history::{self, HistoryKind, NewOperation},
};

use super::{AppState, MAX_IMPORT_RECEIPTS};

impl AppState {
    pub fn start_import(
        &self,
        app: AppHandle,
        request: ImportRequest,
    ) -> AppResult<ImportSnapshot> {
        self.start_import_inner(app, request, false)
            .map(|(snapshot, _)| snapshot)
    }

    /// MCP 导入：与界面导入同一条管线，固定不压缩，并记录 AI 前缀与可取消任务。
    pub fn start_mcp_import(
        &self,
        mut request: ImportRequest,
    ) -> AppResult<(ImportSnapshot, Option<i64>)> {
        request.optimize = None;
        self.start_import_inner(self.app.clone(), request, true)
    }

    fn start_import_inner(
        &self,
        app: AppHandle,
        request: ImportRequest,
        from_mcp: bool,
    ) -> AppResult<(ImportSnapshot, Option<i64>)> {
        let request = request.normalized()?;

        let (database, thumbs, root) = self
            .library_handles()
            .ok_or_else(|| AppError::message("尚未打开图库"))?;

        let temp_target = request.target == ImportTarget::Temp;
        let target_path = if temp_target {
            database
                .with_reader(crate::library::temp::path)?
                .ok_or_else(|| AppError::message("临时文件夹不存在"))?
        } else {
            request.folder.clone()
        };
        let resource_request = if temp_target {
            // 临时导入不写操作记录，也不占用时间线，撤销/重做无需等它。
            ResourceRequest::session_read()
                .with_content_writer()
                .with_paths(vec![PathScope::read(&target_path)])
                .with_path_note("正在导入到")
        } else {
            ResourceRequest::session_read()
                .with_timeline(Access::Read)
                .with_content_writer()
                .with_paths(vec![PathScope::read(&target_path)])
                .with_path_note("正在导入到")
        };
        let scope = if temp_target {
            format!("{} 个路径 · 临时", request.paths.len())
        } else {
            format!("{} 个路径", request.paths.len())
        };
        let job = Arc::new(ImportJob::new(
            uuid::Uuid::new_v4().to_string(),
            scope,
            request.target,
        ));
        let handle = job.handle();
        let runner_job = job.clone();
        let runner_app = app.clone();
        let label = if temp_target {
            format!("导入 {} 个路径到临时区", request.paths.len())
        } else if request.folder.is_empty() {
            format!("导入 {} 个路径", request.paths.len())
        } else {
            format!(
                "导入 {} 个路径到「{}」",
                request.paths.len(),
                request.folder
            )
        };
        let label = if from_mcp {
            format!("AI · {label}")
        } else {
            label
        };
        // 临时导入是生命周期数据：不写操作记录，也不影响撤销/重做时间线。
        let operation = if temp_target {
            None
        } else {
            Some(history::LiveOperation::begin(
                &database,
                NewOperation::new(HistoryKind::Import, label),
            )?)
        };
        let operation_id = operation.as_ref().map(history::LiveOperation::id);
        handle.set_operation_id(operation_id);
        let source = ImportSource {
            database: database.clone(),
            thumbs,
            root,
            operation_id,
            resources: self.resources(),
            images: self.images(),
            memory: self.memory(),
            holder: HolderInfo::new(job.id(), "导入")
                .with_reason("正在导入")
                .with_path_note("正在导入到"),
        };
        let started = self.tasks.launch(
            app,
            handle,
            resource_request,
            ConflictPolicy::Queue,
            move |guard| {
                crate::import::start(runner_app, source, runner_job, request, guard, operation);
            },
        )?;
        if let TaskStartResult::Conflict { active } = started {
            return Err(conflict_error(&active));
        }
        {
            let mut jobs = self.import_jobs.lock();
            jobs.push_back(job.clone());
            while jobs.len() > MAX_IMPORT_RECEIPTS {
                let Some(index) = jobs.iter().position(|job| !job.is_running()) else {
                    break;
                };
                jobs.remove(index);
            }
        }
        if from_mcp {
            self.remember_mcp_task(job.id());
        }
        Ok((job.snapshot(), operation_id))
    }

    /// 导入结束后由导入线程登记；任务列表按任务 id 取回统计与预览。
    pub fn remember_import_receipt(&self, snapshot: ImportSnapshot) {
        let mut receipts = self.import_receipts.lock();
        receipts.retain(|receipt| receipt.progress.job_id != snapshot.progress.job_id);
        receipts.push_front(snapshot);
        receipts.truncate(MAX_IMPORT_RECEIPTS);
    }

    /// 进行中的导入取实时快照，已结束的取回执。
    pub fn import_receipt(&self, job_id: &str) -> Option<ImportSnapshot> {
        self.import_status(job_id).or_else(|| {
            self.import_receipts
                .lock()
                .iter()
                .find(|receipt| receipt.progress.job_id == job_id)
                .cloned()
        })
    }

    pub fn cancel_import(&self, job_id: &str) -> AppResult<()> {
        let job = self
            .import_jobs
            .lock()
            .iter()
            .find(|job| job.id() == job_id)
            .cloned()
            .ok_or_else(|| AppError::message("导入任务不存在或已结束"))?;
        if !job.is_running() {
            return Err(AppError::message("导入任务已经结束"));
        }
        job.request_cancel();
        // 排队中的导入不会自己看到取消标记，还要从任务总线的等待队列里醒来。
        let _ = self.tasks.cancel(job.id());
        Ok(())
    }

    pub fn import_status(&self, job_id: &str) -> Option<ImportSnapshot> {
        self.import_jobs
            .lock()
            .iter()
            .find(|job| job.id() == job_id)
            .map(|job| job.snapshot())
    }
}

fn conflict_error(active: &[TaskSummary]) -> AppError {
    let label = active
        .first()
        .map(|task| task.label.as_str())
        .unwrap_or("其他任务");
    AppError::message(format!("「{label}」正在进行，请等待完成或取消"))
}

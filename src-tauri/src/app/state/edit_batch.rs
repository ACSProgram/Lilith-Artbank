//! 批量编辑任务：准入、资源、进度、失败清单与记录收尾。

use tauri::AppHandle;

use crate::{
    app::{
        error::{AppError, AppResult},
        tasks::{
            BatchTaskRequest, LaneGuard, TaskHandle, TaskKind, TaskPhase, TaskScope,
            TaskStartResult,
        },
    },
    concurrency::resources::{Access, ResourceRequest},
    library::{edit_batch, query},
};

use super::AppState;

impl AppState {
    pub(super) fn start_edit_task(
        &self,
        app: AppHandle,
        request: BatchTaskRequest,
    ) -> AppResult<TaskStartResult> {
        let options = request
            .edit
            .clone()
            .ok_or_else(|| AppError::message("请提供批量编辑参数"))?;
        options.validate()?;
        let TaskScope::Selected { .. } = &request.scope else {
            return Err(AppError::message(
                "批量编辑只接受选中的图片；当前视图、文件夹或全库范围请先缩小选择",
            ));
        };

        let (database, thumbs, root) = self.edit_handles()?;
        let ids = request.scope.resolve(&database)?;
        if ids.is_empty() {
            return Err(AppError::message("没有选中的图片"));
        }
        if options.reversible && ids.len() > edit_batch::MAX_REVERSIBLE_IMAGES {
            return Err(AppError::message(format!(
                "超过 {} 张的批量编辑不能保留撤销副本；请分批处理，或不保留撤销继续",
                edit_batch::MAX_REVERSIBLE_IMAGES
            )));
        }
        let preview = edit_batch::preview(&database, &root, &ids, &options)?;
        edit_batch::ensure_disk_space(&preview)?;
        if preview.will_process == 0 {
            return Err(AppError::message("所选图片都不需要编辑"));
        }

        let handle = TaskHandle::new(
            uuid::Uuid::new_v4().to_string(),
            TaskKind::Edit,
            format!("{} 张", ids.len()),
        );
        let worker = handle.clone();
        let tasks = self.tasks.clone();
        let resources = self.resources();
        let images = self.images();
        let ai = self.ai.clone();
        let skip_breakdown = preview.skip_breakdown.clone();
        // 与导入、压缩、移动互斥；查看器仍可读，单张文件夹写锁在循环内抢占。
        let resource = ResourceRequest::session_read()
            .with_timeline(Access::Read)
            .with_content_writer()
            .with_path_note("正在编辑图片内容");
        self.tasks
            .launch(app, handle, resource, request.conflict, move |guard| {
                let _ = tasks.spawn_worker("batch-edit".to_owned(), worker.clone(), move || {
                    let _guard: LaneGuard = guard;
                    worker.set_phase(TaskPhase::Running);
                    worker.set_total(ids.len());
                    worker.set_skip_breakdown(&skip_breakdown);
                    let operation = match edit_batch::operation(&database, ids.len()) {
                        Ok(operation) => operation,
                        Err(error) => {
                            worker.set_message(error.to_string());
                            worker.finish(TaskPhase::Failed);
                            return;
                        }
                    };
                    let operation_id = operation.id();
                    worker.set_operation_id(Some(operation_id));
                    let result = edit_batch::run(
                        &database,
                        &root,
                        &thumbs,
                        &ids,
                        &options,
                        operation_id,
                        &*worker,
                        &resources,
                        &images,
                    );
                    query::clear_page_cache();
                    ai.invalidate_similarity();
                    match result {
                        Ok(report) => {
                            for failure in &report.failures {
                                worker.add_failure(
                                    Some(failure.image_id),
                                    &failure.path,
                                    &failure.reason,
                                );
                            }
                            worker.set_failed(report.failed as usize);
                            worker.set_skipped(report.skipped as usize);
                            worker.set_skip_breakdown(&report.skip_breakdown);
                            let note = if report.cancelled { "已取消" } else { "" };
                            if let Err(error) = operation.finish(note) {
                                log::warn!("收尾批量编辑记录失败：{error}");
                            }
                            let mut message = format!("已编辑 {} 张", report.processed);
                            if report.skipped > 0 {
                                message.push_str(&format!(" · 跳过 {} 张", report.skipped));
                            }
                            if report.failed > 0 {
                                message.push_str(&format!(" · 失败 {} 张", report.failed));
                            }
                            worker.set_message(message);
                            let phase = if report.cancelled {
                                TaskPhase::Cancelled
                            } else if report.failed > 0 {
                                TaskPhase::PartialFailed
                            } else {
                                TaskPhase::Completed
                            };
                            worker.finish(phase);
                        }
                        Err(error) => {
                            let message = error.to_string();
                            if let Err(error) = operation.finish(&message) {
                                log::warn!("收尾批量编辑记录失败：{error}");
                            }
                            worker.set_message(message);
                            worker.finish(TaskPhase::Failed);
                        }
                    }
                });
            })
    }
}

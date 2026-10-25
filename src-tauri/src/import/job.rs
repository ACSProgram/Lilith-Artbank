use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{
    app::tasks::{TaskHandle, TaskKind, TaskPhase},
    library::service::now_ms,
};

const MAX_FAILURES: usize = 200;
const MAX_HINTS: usize = 100;
const MAX_PREVIEWS: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum DuplicatePolicy {
    #[default]
    Skip,
    Merge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ImportPhase {
    Scanning,
    Processing,
    Finishing,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportFailure {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportHint {
    pub image_id: i64,
    pub similar_to: i64,
    pub distance: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgress {
    pub job_id: String,
    pub phase: ImportPhase,
    pub total: usize,
    pub processed: usize,
    pub imported: usize,
    pub duplicates: usize,
    pub merged: usize,
    pub failed: usize,
    pub similar: usize,
    /// 勾选「导入时压缩」后实际被替换为压缩版本的新增图片数。
    pub optimized: usize,
    /// 这些图片省下的字节数。
    pub optimized_bytes: u64,
    pub current: String,
    pub elapsed_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSnapshot {
    #[serde(flatten)]
    pub progress: ImportProgress,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub failures: Vec<ImportFailure>,
    pub hints: Vec<ImportHint>,
    pub preview_ids: Vec<i64>,
}

/// 可跨线程共享的导入任务状态。进度与取消由通用任务句柄统一承载，
/// 这里保留导入独有的计数（新增/重复/合并/疑似重复）与预览 id。
pub struct ImportJob {
    handle: Arc<TaskHandle>,
    phase: Mutex<ImportPhase>,
    total: AtomicUsize,
    processed: AtomicUsize,
    imported: AtomicUsize,
    duplicates: AtomicUsize,
    merged: AtomicUsize,
    failed: AtomicUsize,
    similar: AtomicUsize,
    optimized: AtomicUsize,
    optimized_bytes: AtomicU64,
    current: Mutex<String>,
    failures: Mutex<Vec<ImportFailure>>,
    hints: Mutex<Vec<ImportHint>>,
    preview_ids: Mutex<Vec<i64>>,
    started_at: i64,
    finished_at: Mutex<Option<i64>>,
}

fn task_phase(phase: ImportPhase) -> TaskPhase {
    match phase {
        ImportPhase::Scanning | ImportPhase::Processing | ImportPhase::Finishing => {
            TaskPhase::Running
        }
        ImportPhase::Completed => TaskPhase::Completed,
        ImportPhase::Cancelled => TaskPhase::Cancelled,
        ImportPhase::Failed => TaskPhase::Failed,
    }
}

impl ImportJob {
    pub fn new(id: impl Into<String>, scope: impl Into<String>) -> Self {
        let id = id.into();
        let handle = TaskHandle::new(id.clone(), TaskKind::Import, scope);
        Self {
            handle,
            phase: Mutex::new(ImportPhase::Scanning),
            total: AtomicUsize::new(0),
            processed: AtomicUsize::new(0),
            imported: AtomicUsize::new(0),
            duplicates: AtomicUsize::new(0),
            merged: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            similar: AtomicUsize::new(0),
            optimized: AtomicUsize::new(0),
            optimized_bytes: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            failures: Mutex::new(Vec::new()),
            hints: Mutex::new(Vec::new()),
            preview_ids: Mutex::new(Vec::new()),
            started_at: now_ms(),
            finished_at: Mutex::new(None),
        }
    }

    pub fn handle(&self) -> Arc<TaskHandle> {
        self.handle.clone()
    }

    pub fn id(&self) -> &str {
        self.handle.id()
    }

    pub fn request_cancel(&self) {
        self.handle.request_cancel();
    }

    pub fn should_cancel(&self) -> bool {
        self.handle.should_cancel()
    }

    pub fn is_running(&self) -> bool {
        self.handle.is_running()
    }

    pub fn phase(&self) -> ImportPhase {
        *self.phase.lock()
    }

    pub fn set_phase(&self, phase: ImportPhase) {
        *self.phase.lock() = phase;
        self.handle.set_phase(task_phase(phase));
    }

    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
        self.handle.set_total(total);
    }

    #[cfg(test)]
    pub fn processed(&self) -> usize {
        self.processed.load(Ordering::Relaxed)
    }

    pub fn inc_processed(&self) {
        self.processed.fetch_add(1, Ordering::Relaxed);
        self.sync_task_progress();
    }

    pub fn inc_imported(&self, count: usize) {
        self.imported.fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_duplicates(&self, count: usize) {
        self.duplicates.fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_merged(&self, count: usize) {
        self.merged.fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.handle.inc_failed();
        self.sync_task_progress();
    }

    /// 失败项也算“已处理”，但只计入失败数：任务条的 `已完成 + 失败` 不能把同一张图算两次。
    fn sync_task_progress(&self) {
        let processed = self.processed.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        self.handle.set_completed(processed.saturating_sub(failed));
    }

    pub fn inc_similar(&self, count: usize) {
        self.similar.fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_optimized(&self, count: usize, saved_bytes: u64) {
        self.optimized.fetch_add(count, Ordering::Relaxed);
        self.optimized_bytes
            .fetch_add(saved_bytes, Ordering::Relaxed);
    }

    pub fn set_current(&self, current: impl Into<String>) {
        let current = current.into();
        self.handle.set_message(current.clone());
        *self.current.lock() = current;
    }

    pub fn add_failure(&self, path: impl Into<String>, reason: impl Into<String>) {
        let path = path.into();
        let reason = reason.into();
        self.handle.add_failure(None, path.clone(), reason.clone());
        let mut failures = self.failures.lock();
        if failures.len() < MAX_FAILURES {
            failures.push(ImportFailure { path, reason });
        }
    }

    pub fn add_hint(&self, hint: ImportHint) {
        let mut hints = self.hints.lock();
        if hints.len() < MAX_HINTS {
            hints.push(hint);
        }
    }

    pub fn add_preview(&self, image_id: i64) {
        let mut previews = self.preview_ids.lock();
        if previews.len() >= MAX_PREVIEWS {
            previews.remove(0);
        }
        previews.push(image_id);
    }

    pub fn finish(&self, phase: ImportPhase) {
        *self.phase.lock() = phase;
        *self.finished_at.lock() = Some(now_ms());
        self.set_current("");
        // 导入会新增图片行，缓存的有序 id 列表随即过期。
        crate::library::query::clear_page_cache();
        self.handle.finish(task_phase(phase));
    }

    pub fn progress(&self) -> ImportProgress {
        ImportProgress {
            job_id: self.handle.id().to_owned(),
            phase: self.phase(),
            total: self.total.load(Ordering::Relaxed),
            processed: self.processed.load(Ordering::Relaxed),
            imported: self.imported.load(Ordering::Relaxed),
            duplicates: self.duplicates.load(Ordering::Relaxed),
            merged: self.merged.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            similar: self.similar.load(Ordering::Relaxed),
            optimized: self.optimized.load(Ordering::Relaxed),
            optimized_bytes: self.optimized_bytes.load(Ordering::Relaxed),
            current: self.current.lock().clone(),
            elapsed_ms: now_ms().saturating_sub(self.started_at),
        }
    }

    pub fn snapshot(&self) -> ImportSnapshot {
        ImportSnapshot {
            progress: self.progress(),
            started_at: self.started_at,
            finished_at: *self.finished_at.lock(),
            failures: self.failures.lock().clone(),
            hints: self.hints.lock().clone(),
            preview_ids: self.preview_ids.lock().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serializes_flat_camel_case_fields() {
        let job = ImportJob::new("job-1", "1 个路径");
        job.set_total(10);
        job.inc_imported(3);
        job.add_failure("a.png", "无法解码");

        let value = serde_json::to_value(job.snapshot()).unwrap();
        assert_eq!(value["jobId"], "job-1");
        assert_eq!(value["phase"], "scanning");
        assert_eq!(value["total"], 10);
        assert_eq!(value["imported"], 3);
        assert_eq!(value["failures"][0]["path"], "a.png");
        assert!(value.get("startedAt").is_some());
        assert!(value.get("finishedAt").is_some());
        assert!(value.get("previewIds").unwrap().is_array());
        assert!(value.get("progress").is_none());
    }

    #[test]
    fn duplicate_policy_uses_camel_case() {
        let skip: DuplicatePolicy = serde_json::from_str("\"skip\"").unwrap();
        let merge: DuplicatePolicy = serde_json::from_str("\"merge\"").unwrap();
        assert_eq!(skip, DuplicatePolicy::Skip);
        assert_eq!(merge, DuplicatePolicy::Merge);
    }
}

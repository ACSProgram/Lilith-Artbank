use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::library::service::now_ms;

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

/// 可跨线程共享的导入任务状态。计数器只增不减，快照按需读取。
pub struct ImportJob {
    id: String,
    cancel: AtomicBool,
    running: AtomicBool,
    phase: Mutex<ImportPhase>,
    total: AtomicUsize,
    processed: AtomicUsize,
    imported: AtomicUsize,
    duplicates: AtomicUsize,
    merged: AtomicUsize,
    failed: AtomicUsize,
    similar: AtomicUsize,
    current: Mutex<String>,
    failures: Mutex<Vec<ImportFailure>>,
    hints: Mutex<Vec<ImportHint>>,
    preview_ids: Mutex<Vec<i64>>,
    started_at: i64,
    finished_at: Mutex<Option<i64>>,
}

impl ImportJob {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            cancel: AtomicBool::new(false),
            running: AtomicBool::new(true),
            phase: Mutex::new(ImportPhase::Scanning),
            total: AtomicUsize::new(0),
            processed: AtomicUsize::new(0),
            imported: AtomicUsize::new(0),
            duplicates: AtomicUsize::new(0),
            merged: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            similar: AtomicUsize::new(0),
            current: Mutex::new(String::new()),
            failures: Mutex::new(Vec::new()),
            hints: Mutex::new(Vec::new()),
            preview_ids: Mutex::new(Vec::new()),
            started_at: now_ms(),
            finished_at: Mutex::new(None),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn phase(&self) -> ImportPhase {
        *self.phase.lock()
    }

    pub fn set_phase(&self, phase: ImportPhase) {
        *self.phase.lock() = phase;
    }

    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn processed(&self) -> usize {
        self.processed.load(Ordering::Relaxed)
    }

    pub fn inc_processed(&self) {
        self.processed.fetch_add(1, Ordering::Relaxed);
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
    }

    pub fn inc_similar(&self, count: usize) {
        self.similar.fetch_add(count, Ordering::Relaxed);
    }

    pub fn set_current(&self, current: impl Into<String>) {
        *self.current.lock() = current.into();
    }

    pub fn add_failure(&self, path: impl Into<String>, reason: impl Into<String>) {
        let mut failures = self.failures.lock();
        if failures.len() < MAX_FAILURES {
            failures.push(ImportFailure {
                path: path.into(),
                reason: reason.into(),
            });
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
        self.set_phase(phase);
        *self.finished_at.lock() = Some(now_ms());
        self.set_current("");
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn progress(&self) -> ImportProgress {
        ImportProgress {
            job_id: self.id.clone(),
            phase: self.phase(),
            total: self.total.load(Ordering::Relaxed),
            processed: self.processed.load(Ordering::Relaxed),
            imported: self.imported.load(Ordering::Relaxed),
            duplicates: self.duplicates.load(Ordering::Relaxed),
            merged: self.merged.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            similar: self.similar.load(Ordering::Relaxed),
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
        let job = ImportJob::new("job-1");
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

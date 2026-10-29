use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{
    app::tasks::{TaskHandle, TaskKind, TaskPhase},
    import::ImportTarget,
    library::service::now_ms,
};

const MAX_FAILURES: usize = 200;
const MAX_HINTS: usize = 100;
/// 四组归因清单（精确重复、批内重复、合并目标、源文件映射）各自最多保留这么多条；
/// 精确总数另计，不受截断影响。
const MAX_PAIRS: usize = 200;
/// 与拼图最大格数一致，导入完成后可以直接拼一张 5×5。
const MAX_PREVIEWS: usize = 25;

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
    Queued,
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

/// 精确重复：某个源文件撞上了库里已有的某张图。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportDuplicatePair {
    pub source_path: String,
    pub image_id: i64,
    pub trashed: bool,
}

/// 批内重复：同一次导入里内容相同的多份源文件只入库一份。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportBatchPair {
    pub source_path: String,
    pub kept_path: String,
}

/// 新增图片的源文件与图库 id 对应关系。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSourcePair {
    pub source_path: String,
    pub image_id: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgress {
    pub job_id: String,
    pub phase: ImportPhase,
    /// 普通导入或临时导入；临时导入不入操作记录。
    pub target: ImportTarget,
    pub total: usize,
    pub processed: usize,
    /// 已被写线程提交（成功、重复或失败）的条数。
    pub written: usize,
    /// 加权进度：处理阶段 0–90，写入阶段 90–100；完成前不超过 99。
    pub percent: u8,
    pub imported: usize,
    pub duplicates: usize,
    pub merged: usize,
    pub failed: usize,
    /// 图库导入时被丢弃的临时区同内容行（这些图改在图库里入库）。
    pub temp_replaced: usize,
    pub similar: usize,
    /// 勾选「导入时压缩」后实际被替换为压缩版本的新增图片数。
    pub optimized: usize,
    /// 这些图片省下的字节数。
    pub optimized_bytes: u64,
    /// 已入库新增图片的最终字节总和（压缩后的按压缩结果算）。
    pub written_bytes: u64,
    /// 进入处理阶段后按已写入张数折算的速度；样本不足或未开始时为空。
    pub rate_per_second: Option<f64>,
    /// 按当前速度估算的剩余秒数；终态为空。
    pub eta_seconds: Option<u64>,
    /// 压缩编码出错、已按原图入库的张数。
    pub optimize_failed: usize,
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
    pub duplicate_pairs: Vec<ImportDuplicatePair>,
    pub duplicate_pair_count: usize,
    pub duplicate_pairs_truncated: bool,
    pub batch_duplicate_pairs: Vec<ImportBatchPair>,
    pub batch_duplicate_pair_count: usize,
    pub batch_duplicate_pairs_truncated: bool,
    pub merged_image_ids: Vec<i64>,
    pub merged_image_ids_truncated: bool,
    pub imported_pairs: Vec<ImportSourcePair>,
    pub imported_pairs_truncated: bool,
}

/// 有上限的归因清单：只保留前 `MAX_PAIRS` 条，总数单独精确计数。
struct PairLog<T> {
    items: Mutex<Vec<T>>,
    count: AtomicUsize,
}

impl<T> PairLog<T> {
    fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
        }
    }

    fn push(&self, item: T) {
        self.count.fetch_add(1, Ordering::Relaxed);
        let mut items = self.items.lock();
        if items.len() < MAX_PAIRS {
            items.push(item);
        }
    }

    fn snapshot(&self) -> (Vec<T>, usize)
    where
        T: Clone,
    {
        let items = self.items.lock().clone();
        let count = self.count.load(Ordering::Relaxed);
        (items, count)
    }
}

/// 可跨线程共享的导入任务状态。进度与取消由通用任务句柄统一承载，
/// 这里保留导入独有的计数（新增/重复/合并/疑似重复）与预览 id。
pub struct ImportJob {
    handle: Arc<TaskHandle>,
    target: ImportTarget,
    phase: Mutex<ImportPhase>,
    total: AtomicUsize,
    processed: AtomicUsize,
    written: AtomicUsize,
    imported: AtomicUsize,
    duplicates: AtomicUsize,
    merged: AtomicUsize,
    failed: AtomicUsize,
    temp_replaced: AtomicUsize,
    similar: AtomicUsize,
    optimized: AtomicUsize,
    optimized_bytes: AtomicU64,
    written_bytes: AtomicU64,
    /// 勾选压缩后编码器报错、按原图入库的张数。
    optimize_failed: AtomicUsize,
    current: Mutex<String>,
    failures: Mutex<Vec<ImportFailure>>,
    hints: Mutex<Vec<ImportHint>>,
    preview_ids: Mutex<Vec<i64>>,
    duplicate_pairs: PairLog<ImportDuplicatePair>,
    batch_duplicate_pairs: PairLog<ImportBatchPair>,
    merged_image_ids: PairLog<i64>,
    imported_pairs: PairLog<ImportSourcePair>,
    started_at: i64,
    finished_at: Mutex<Option<i64>>,
}

fn task_phase(phase: ImportPhase) -> TaskPhase {
    match phase {
        ImportPhase::Queued => TaskPhase::Queued,
        ImportPhase::Scanning | ImportPhase::Processing | ImportPhase::Finishing => {
            TaskPhase::Running
        }
        ImportPhase::Completed => TaskPhase::Completed,
        ImportPhase::Cancelled => TaskPhase::Cancelled,
        ImportPhase::Failed => TaskPhase::Failed,
    }
}

impl ImportJob {
    pub fn new(id: impl Into<String>, scope: impl Into<String>, target: ImportTarget) -> Self {
        let id = id.into();
        let handle = TaskHandle::new(id.clone(), TaskKind::Import, scope);
        handle.set_import_target(target);
        Self {
            handle,
            target,
            phase: Mutex::new(ImportPhase::Queued),
            total: AtomicUsize::new(0),
            processed: AtomicUsize::new(0),
            written: AtomicUsize::new(0),
            imported: AtomicUsize::new(0),
            duplicates: AtomicUsize::new(0),
            merged: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            temp_replaced: AtomicUsize::new(0),
            similar: AtomicUsize::new(0),
            optimized: AtomicUsize::new(0),
            optimized_bytes: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            optimize_failed: AtomicUsize::new(0),
            current: Mutex::new(String::new()),
            failures: Mutex::new(Vec::new()),
            hints: Mutex::new(Vec::new()),
            preview_ids: Mutex::new(Vec::new()),
            duplicate_pairs: PairLog::new(),
            batch_duplicate_pairs: PairLog::new(),
            merged_image_ids: PairLog::new(),
            imported_pairs: PairLog::new(),
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

    /// 写线程提交（或确认失败）后调用；`written` 才是真正离开缓冲区、落库的条数。
    pub fn inc_written(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.written.fetch_add(count, Ordering::Relaxed);
        self.sync_task_progress();
    }

    pub fn inc_imported(&self, count: usize) {
        if count == 0 {
            return;
        }
        let total = self.imported.fetch_add(count, Ordering::Relaxed) + count;
        // 任务快照带上新增数量，前端刷新时可以直接改计数，不必重查全库。
        self.handle.set_added_images(total);
    }

    /// 写线程提交一批新增图片后累加它们的最终字节数。
    pub fn inc_written_bytes(&self, bytes: u64) {
        self.written_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn inc_duplicates(&self, count: usize) {
        self.duplicates.fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_merged(&self, count: usize) {
        self.merged.fetch_add(count, Ordering::Relaxed);
    }

    /// 图库导入把同内容的临时行丢弃后，这张图是正常入库的，不算重复。
    pub fn inc_temp_replaced(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.temp_replaced.fetch_add(count, Ordering::Relaxed);
        self.handle.inc_removed_temp_images(count);
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
        self.handle.set_percent(self.percent());
    }

    fn percent(&self) -> u8 {
        let phase = self.effective_phase();
        if phase == ImportPhase::Completed {
            return 100;
        }
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0;
        }
        let processed = self.processed.load(Ordering::Relaxed);
        let written = self.written.load(Ordering::Relaxed);
        let percent = if phase == ImportPhase::Finishing {
            // 工作线程已结束，剩下的是写线程把缓冲区排空。
            90 + written.min(total).saturating_mul(10) / total
        } else {
            processed.saturating_mul(90) / total
        };
        (percent as u8).min(99)
    }

    pub fn inc_similar(&self, count: usize) {
        self.similar.fetch_add(count, Ordering::Relaxed);
    }

    pub fn inc_optimized(&self, count: usize, saved_bytes: u64) {
        self.optimized.fetch_add(count, Ordering::Relaxed);
        self.optimized_bytes
            .fetch_add(saved_bytes, Ordering::Relaxed);
    }

    /// 编码器异常而不是判据不满足：图片按原图入库，但要让人知道压缩没成功。
    pub fn inc_optimize_failed(&self) {
        self.optimize_failed.fetch_add(1, Ordering::Relaxed);
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

    pub fn add_duplicate_pair(&self, source_path: String, image_id: i64, trashed: bool) {
        self.duplicate_pairs.push(ImportDuplicatePair {
            source_path,
            image_id,
            trashed,
        });
    }

    pub fn add_batch_duplicate_pair(&self, source_path: String, kept_path: String) {
        self.batch_duplicate_pairs.push(ImportBatchPair {
            source_path,
            kept_path,
        });
    }

    pub fn add_merged_image(&self, image_id: i64) {
        self.merged_image_ids.push(image_id);
    }

    pub fn add_imported_pair(&self, source_path: String, image_id: i64) {
        self.imported_pairs.push(ImportSourcePair {
            source_path,
            image_id,
        });
    }

    pub fn finish(&self, phase: ImportPhase) {
        *self.phase.lock() = phase;
        *self.finished_at.lock() = Some(now_ms());
        self.set_current("");
        // 导入会新增图片行，缓存的有序 id 列表随即过期。
        crate::library::query::clear_page_cache();
        self.handle.finish(task_phase(phase));
    }

    /// 排队中被取消或线程没起来时，导入阶段还停在 Queued；以任务总线的终态为准。
    fn effective_phase(&self) -> ImportPhase {
        let phase = self.phase();
        if phase != ImportPhase::Queued {
            return phase;
        }
        match self.handle.phase() {
            TaskPhase::Cancelled => ImportPhase::Cancelled,
            TaskPhase::Failed => ImportPhase::Failed,
            _ => ImportPhase::Queued,
        }
    }

    pub fn progress(&self) -> ImportProgress {
        let phase = self.effective_phase();
        let total = self.total.load(Ordering::Relaxed);
        let written = self.written.load(Ordering::Relaxed);
        // 与任务总线同一口径：运行时间从第一次进入 Running 算起，样本不足时不给速度。
        let rate_per_second = self.handle.run_started_at().and_then(|start| {
            let finished_at = *self.finished_at.lock();
            let elapsed = (finished_at.unwrap_or_else(now_ms) - start) as f64 / 1000.0;
            (written > 0 && elapsed >= 0.5).then(|| written as f64 / elapsed)
        });
        let terminal = matches!(
            phase,
            ImportPhase::Completed | ImportPhase::Cancelled | ImportPhase::Failed
        );
        let eta_seconds = if terminal {
            None
        } else {
            rate_per_second.and_then(|rate| {
                let remaining = total.saturating_sub(written);
                (remaining > 0 && rate > 0.0)
                    .then(|| ((remaining as f64 / rate).ceil() as u64).max(1))
            })
        };

        ImportProgress {
            job_id: self.handle.id().to_owned(),
            phase,
            target: self.target,
            total,
            processed: self.processed.load(Ordering::Relaxed),
            written,
            percent: self.percent(),
            imported: self.imported.load(Ordering::Relaxed),
            duplicates: self.duplicates.load(Ordering::Relaxed),
            merged: self.merged.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            temp_replaced: self.temp_replaced.load(Ordering::Relaxed),
            similar: self.similar.load(Ordering::Relaxed),
            optimized: self.optimized.load(Ordering::Relaxed),
            optimized_bytes: self.optimized_bytes.load(Ordering::Relaxed),
            written_bytes: self.written_bytes.load(Ordering::Relaxed),
            rate_per_second,
            eta_seconds,
            optimize_failed: self.optimize_failed.load(Ordering::Relaxed),
            current: self.current.lock().clone(),
            elapsed_ms: now_ms().saturating_sub(self.started_at),
        }
    }

    pub fn snapshot(&self) -> ImportSnapshot {
        let (duplicate_pairs, duplicate_pair_count) = self.duplicate_pairs.snapshot();
        let (batch_duplicate_pairs, batch_duplicate_pair_count) =
            self.batch_duplicate_pairs.snapshot();
        let (merged_image_ids, merged_count) = self.merged_image_ids.snapshot();
        let (imported_pairs, imported_count) = self.imported_pairs.snapshot();
        ImportSnapshot {
            progress: self.progress(),
            started_at: self.started_at,
            finished_at: *self.finished_at.lock(),
            failures: self.failures.lock().clone(),
            hints: self.hints.lock().clone(),
            preview_ids: self.preview_ids.lock().clone(),
            duplicate_pairs_truncated: duplicate_pair_count > duplicate_pairs.len(),
            duplicate_pairs,
            duplicate_pair_count,
            batch_duplicate_pairs_truncated: batch_duplicate_pair_count
                > batch_duplicate_pairs.len(),
            batch_duplicate_pairs,
            batch_duplicate_pair_count,
            merged_image_ids_truncated: merged_count > merged_image_ids.len(),
            merged_image_ids,
            imported_pairs_truncated: imported_count > imported_pairs.len(),
            imported_pairs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serializes_flat_camel_case_fields() {
        let job = ImportJob::new("job-1", "1 个路径", ImportTarget::Library);
        job.set_total(10);
        job.inc_imported(3);
        job.add_failure("a.png", "无法解码");

        let value = serde_json::to_value(job.snapshot()).unwrap();
        assert_eq!(value["jobId"], "job-1");
        assert_eq!(value["phase"], "queued");
        assert_eq!(value["total"], 10);
        assert_eq!(value["imported"], 3);
        assert_eq!(value["failures"][0]["path"], "a.png");
        assert!(value.get("startedAt").is_some());
        assert!(value.get("finishedAt").is_some());
        assert!(value.get("previewIds").unwrap().is_array());
        assert!(value.get("progress").is_none());
    }

    #[test]
    fn percent_uses_processing_band_until_finishing() {
        let job = ImportJob::new("job-percent", "1 个路径", ImportTarget::Library);
        job.set_total(10);
        job.set_phase(ImportPhase::Processing);
        assert_eq!(job.progress().percent, 0, "刚开始不能直接跳到写入段");

        for _ in 0..5 {
            job.inc_processed();
        }
        assert_eq!(job.progress().percent, 45);

        // 写线程在解码阶段并行推进，不把处理阶段的进度推入 90–99。
        job.inc_written(5);
        assert_eq!(job.progress().percent, 45);

        for _ in 0..5 {
            job.inc_processed();
        }
        job.set_phase(ImportPhase::Finishing);
        assert_eq!(job.progress().percent, 95);

        job.inc_written(5);
        job.set_phase(ImportPhase::Completed);
        assert_eq!(job.progress().percent, 100);
    }

    #[test]
    fn progress_reports_written_bytes_rate_and_eta() {
        let job = ImportJob::new("job-rate", "1 个路径", ImportTarget::Library);
        job.set_total(10);
        job.set_phase(ImportPhase::Processing);
        job.inc_written(2);
        job.inc_written_bytes(1024);
        std::thread::sleep(std::time::Duration::from_millis(600));

        let progress = job.progress();
        assert_eq!(progress.written_bytes, 1024);
        assert!(progress.rate_per_second.is_some_and(|rate| rate > 0.0));
        assert!(progress.eta_seconds.is_some());

        job.set_phase(ImportPhase::Completed);
        let finished = job.progress();
        assert!(finished.eta_seconds.is_none());
        assert!(finished.rate_per_second.is_some());
    }

    #[test]
    fn queued_cancel_uses_task_terminal_phase() {
        let job = ImportJob::new("job-queued", "1 个路径", ImportTarget::Library);
        assert_eq!(job.progress().phase, ImportPhase::Queued);
        job.handle().finish(TaskPhase::Cancelled);
        assert_eq!(job.progress().phase, ImportPhase::Cancelled);
    }

    #[test]
    fn preview_ids_keep_the_latest_25() {
        let job = ImportJob::new("job-1", "1 个路径", ImportTarget::Library);
        for id in 1..=30 {
            job.add_preview(id);
        }
        let previews = job.snapshot().preview_ids;
        assert_eq!(previews.len(), MAX_PREVIEWS);
        assert_eq!(previews.first(), Some(&6));
        assert_eq!(previews.last(), Some(&30));
    }

    #[test]
    fn duplicate_policy_uses_camel_case() {
        let skip: DuplicatePolicy = serde_json::from_str("\"skip\"").unwrap();
        let merge: DuplicatePolicy = serde_json::from_str("\"merge\"").unwrap();
        assert_eq!(skip, DuplicatePolicy::Skip);
        assert_eq!(merge, DuplicatePolicy::Merge);
    }
}

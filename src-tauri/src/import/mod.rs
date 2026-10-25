pub mod decode;
pub mod job;
pub mod pipeline;
pub mod scan;

use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};

use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::{
    app::{
        error::{AppError, AppResult},
        state::AppState,
        tasks::LaneGuard,
    },
    library::{db::LibraryDb, history, paths::normalize_folder, thumbs::ThumbPack},
};

use job::{DuplicatePolicy, ImportJob, ImportPhase};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FolderMode {
    /// 所有图片进入目标文件夹。
    #[default]
    Flat,
    /// 保留每个输入目录下的相对目录层级。
    Preserve,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRequest {
    pub paths: Vec<String>,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub folder_mode: FolderMode,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub score: u8,
    #[serde(default)]
    pub duplicate_policy: DuplicatePolicy,
    #[serde(default = "default_threshold")]
    pub perceptual_threshold: u8,
    #[serde(default)]
    pub worker_threads: Option<usize>,
    #[cfg(test)]
    #[serde(skip)]
    pub cancel_after: Option<usize>,
}

fn default_threshold() -> u8 {
    6
}

impl ImportRequest {
    /// 校验并规范化用户输入；路径必须是绝对路径，文件夹必须是图库内相对路径。
    pub fn normalized(mut self) -> AppResult<Self> {
        self.paths = self
            .paths
            .into_iter()
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty())
            .collect();
        if self.paths.is_empty() {
            return Err(AppError::message("请先选择要导入的文件或目录"));
        }
        if self.paths.len() > 10_000 {
            return Err(AppError::message("单次最多选择 10000 个输入路径"));
        }
        if let Some(path) = self
            .paths
            .iter()
            .find(|path| !std::path::Path::new(path).is_absolute())
        {
            return Err(AppError::message(format!("导入路径必须是绝对路径：{path}")));
        }

        self.folder = normalize_folder(&self.folder)?;
        if self.score > 5 {
            return Err(AppError::message("评分必须在 0-5 之间"));
        }
        self.perceptual_threshold = self.perceptual_threshold.min(64);

        let mut seen = HashSet::new();
        let mut tags = Vec::new();
        for tag in self.tags {
            let tag = tag.trim().to_owned();
            if tag.is_empty() {
                continue;
            }
            if tag.chars().count() > 64 {
                return Err(AppError::message("单个标签不能超过 64 个字符"));
            }
            if tags.len() >= 32 {
                return Err(AppError::message("单次导入最多设置 32 个标签"));
            }
            if seen.insert(tag.to_lowercase()) {
                tags.push(tag);
            }
        }
        self.tags = tags;
        Ok(self)
    }
}

pub struct ImportSource {
    pub database: Arc<LibraryDb>,
    pub thumbs: Arc<ThumbPack>,
    pub root: PathBuf,
    /// 本次导入的操作记录 id；为 `Some` 时新增行写入 `origin_operation_id` 并追加逆数据。
    pub operation_id: Option<i64>,
}

/// 启动后台导入；立即返回，进度与最终结果通过事件推送。
/// `guard` 是任务总线上的 write lane 占用权，随导入线程一起释放。
pub fn start(
    app: AppHandle,
    source: ImportSource,
    job: Arc<ImportJob>,
    request: ImportRequest,
    guard: LaneGuard,
) {
    spawn_progress(app.clone(), job.clone());

    let operation_id = source.operation_id;
    let history_database = source.database.clone();
    let history_database_fallback = history_database.clone();
    let history_app = app.clone();
    let fallback_app = app.clone();
    let fallback_job = job.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("import-{}", job.id()))
        .spawn(move || {
            let _guard = guard;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pipeline::run(source, job.clone(), request)
            }));

            let phase = if job.should_cancel() {
                ImportPhase::Cancelled
            } else {
                match outcome {
                    Ok(Ok(())) => ImportPhase::Completed,
                    Ok(Err(error)) => {
                        job.add_failure("", error.to_string());
                        job.inc_failed();
                        ImportPhase::Failed
                    }
                    Err(_) => {
                        log::error!("导入任务异常终止");
                        job.add_failure("", "导入任务异常终止");
                        job.inc_failed();
                        ImportPhase::Failed
                    }
                }
            };

            job.finish(phase);
            if let Some(op_id) = operation_id {
                let error = if phase == ImportPhase::Failed {
                    "导入未完成"
                } else {
                    ""
                };
                if let Err(error) = history::finish_live(&history_database, op_id, error) {
                    log::warn!("收尾导入操作记录失败：{error}");
                }
            }
            if let Some(state) = history_app.try_state::<AppState>() {
                state.after_background_write();
            }
            let _ = app.emit("import-progress", job.progress());
            let _ = app.emit("import-finished", job.snapshot());
        });

    if let Err(error) = spawn {
        log::error!("无法启动导入线程：{error}");
        if let Some(op_id) = operation_id {
            // 还没有任何批次提交，finish_live 会删掉空记录。
            let _ = history::finish_live(&history_database_fallback, op_id, "导入线程启动失败");
        }
        fallback_job.add_failure("", format!("无法启动导入任务：{error}"));
        fallback_job.inc_failed();
        fallback_job.finish(ImportPhase::Failed);
        let _ = fallback_app.emit("import-finished", fallback_job.snapshot());
    }
}

fn spawn_progress(app: AppHandle, job: Arc<ImportJob>) {
    std::thread::spawn(move || {
        loop {
            let _ = app.emit("import-progress", job.progress());
            if !job.is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ImportRequest {
        ImportRequest {
            paths: vec![std::env::temp_dir().to_string_lossy().into_owned()],
            folder: String::new(),
            folder_mode: FolderMode::Flat,
            tags: Vec::new(),
            score: 0,
            duplicate_policy: DuplicatePolicy::Skip,
            perceptual_threshold: 6,
            worker_threads: None,
            cancel_after: None,
        }
    }

    #[test]
    fn normalizes_folder_tags_and_threshold() {
        let mut value = request();
        value.folder = " 人物\\速写 ".into();
        value.tags = vec![" 速写 ".into(), "速写".into(), "人物".into(), "  ".into()];
        value.perceptual_threshold = 200;
        let normalized = value.normalized().unwrap();
        assert_eq!(normalized.folder, "人物/速写");
        assert_eq!(normalized.tags, vec!["速写".to_owned(), "人物".to_owned()]);
        assert_eq!(normalized.perceptual_threshold, 64);
    }

    #[test]
    fn rejects_relative_paths_and_bad_scores() {
        let mut value = request();
        value.paths = vec!["relative.png".into()];
        assert!(value.normalized().is_err());

        let mut value = request();
        value.score = 6;
        assert!(value.normalized().is_err());

        let mut value = request();
        value.folder = "../外面".into();
        assert!(value.normalized().is_err());
    }
}

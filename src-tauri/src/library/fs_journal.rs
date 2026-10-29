//! 写路径的文件补偿：把已经落地的文件系统动作记下来，事务失败时逆序撤回。
//!
//! 文件动作与数据库事务没有共同的原子边界。写闭包里的 `rename`、发布新文件、
//! 建目录一旦成功，后面任何一步失败（业务错误、SQL 失败、提交失败、panic 展开）
//! 都会让磁盘与行指向分叉。这里的约定是：
//!
//! - 每完成一个文件动作就记一步；
//! - 事务成功提交后 `disarm()`，什么都不做；
//! - 其余任何退出路径都在析构时逆序撤回。
//!
//! 撤回本身失败只记日志，不掩盖原始错误：能回去的都回去，回不去的留在日志里。
//! 撤销/重做走的是同一套步骤，但它们按记录逐条处理，失败时由调用方显式
//! `rollback()`，所以默认不装填（`Default` 得到的是未装填的日志）。

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::app::error::{AppError, AppResult};

enum Step {
    /// 文件或目录从 `from` 搬到了 `to`。
    Renamed { from: PathBuf, to: PathBuf },
    /// 新建了一个目录；撤回时删掉（非空则保留，只记日志）。
    Created(PathBuf),
    /// 新建了一棵只属于本次操作的目录树（例如撤销暂存目录）；撤回时整棵删掉。
    CreatedTree(PathBuf),
    /// 删除了一个空目录；撤回时重建。
    Removed(PathBuf),
    /// 发布了一个新文件；撤回时删掉它。
    Published(PathBuf),
}

#[derive(Default)]
pub(crate) struct FileJournal {
    steps: Vec<Step>,
    armed: bool,
}

impl FileJournal {
    /// 装填的日志：析构时如果还没 `disarm()`，就把记下的动作逆序撤回。
    pub(crate) fn armed() -> Self {
        Self {
            steps: Vec::new(),
            armed: true,
        }
    }

    pub(crate) fn renamed(&mut self, from: &Path, to: &Path) {
        self.steps.push(Step::Renamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
    }

    pub(crate) fn created(&mut self, path: &Path) {
        self.steps.push(Step::Created(path.to_path_buf()));
    }

    pub(crate) fn created_tree(&mut self, path: &Path) {
        self.steps.push(Step::CreatedTree(path.to_path_buf()));
    }

    pub(crate) fn removed(&mut self, path: &Path) {
        self.steps.push(Step::Removed(path.to_path_buf()));
    }

    pub(crate) fn published(&mut self, path: &Path) {
        self.steps.push(Step::Published(path.to_path_buf()));
    }

    /// 递归建目录，把本次真正新建的每一级都记下来；回滚时从深到浅删。
    ///
    /// 逐级 `create_dir` 而不是一次 `create_dir_all`：中途失败时，已经建好的层级
    /// 也已经记进日志，不会留下一截没人管的目录。路径上已有的同名文件按失败处理，
    /// 与 `create_dir_all` 一致。
    pub(crate) fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        let mut missing = Vec::new();
        let mut current = Some(path);
        while let Some(directory) = current {
            if directory.as_os_str().is_empty() {
                break;
            }
            match fs::metadata(directory) {
                Ok(metadata) if metadata.is_dir() => break,
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} 已存在且不是目录", directory.display()),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    missing.push(directory.to_path_buf());
                }
                Err(error) => return Err(error),
            }
            current = directory.parent();
        }
        for directory in missing.iter().rev() {
            fs::create_dir(directory)?;
            self.steps.push(Step::Created(directory.clone()));
        }
        Ok(())
    }

    /// 逆序撤回全部动作；可以重复调用，撤回过的步骤不会再来一遍。
    pub(crate) fn rollback(&mut self) {
        for step in self.steps.drain(..).rev() {
            match step {
                Step::Renamed { from, to } => {
                    let case_only = to
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&from.to_string_lossy());
                    if let Err(error) = move_path(&to, &from, case_only) {
                        log::error!("回滚文件搬移失败：{error}；文件位于 {}", to.display());
                    }
                }
                Step::Created(path) => {
                    if let Err(error) = fs::remove_dir(&path) {
                        log::warn!("回滚目录新建失败：{error}；目录位于 {}", path.display());
                    }
                }
                Step::CreatedTree(path) => {
                    if let Err(error) = fs::remove_dir_all(&path) {
                        log::warn!("回滚临时目录失败：{error}；目录位于 {}", path.display());
                    }
                }
                Step::Removed(path) => {
                    if let Err(error) = fs::create_dir_all(&path) {
                        log::error!("回滚目录删除失败：{error}；目录位于 {}", path.display());
                    }
                }
                Step::Published(path) => {
                    if let Err(error) = fs::remove_file(&path)
                        && error.kind() != io::ErrorKind::NotFound
                    {
                        log::warn!("回滚文件发布失败：{error}；文件位于 {}", path.display());
                    }
                }
            }
        }
    }

    /// 事务已经提交：清空步骤，析构时不再撤回。
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
        self.steps.clear();
    }
}

impl Drop for FileJournal {
    fn drop(&mut self) {
        if self.armed {
            self.rollback();
        }
    }
}

/// 目录改名；`case_only` 表示新旧路径只差大小写。
///
/// Windows 不允许直接把 `Foo` 改成 `foo`（目标被视为已存在），用临时名中转。
pub(crate) fn rename_path(from: &Path, to: &Path, case_only: bool) -> AppResult<()> {
    move_path(from, to, case_only).map_err(AppError::from)
}

/// 同卷用 rename；跨卷（`EXDEV`）退化为复制 + 删源。目录不做跨卷回退。
pub(crate) fn move_file(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device(&error) && source.is_file() => {
            fs::copy(source, destination)?;
            fs::remove_file(source)?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn move_path(from: &Path, to: &Path, case_only: bool) -> io::Result<()> {
    if !case_only {
        return move_file(from, to);
    }
    let parent = from.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".artbank-rename-{}", uuid::Uuid::new_v4()));
    fs::rename(from, &temporary)?;
    match fs::rename(&temporary, to) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::rename(&temporary, from);
            Err(error)
        }
    }
}

#[cfg(unix)]
fn is_cross_device(error: &io::Error) -> bool {
    error.raw_os_error() == Some(18)
}

#[cfg(not(unix))]
fn is_cross_device(_error: &io::Error) -> bool {
    // Windows 的 `MoveFileEx` 在跨卷时由标准库自行退化，不会返回这个错误。
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, name.as_bytes()).unwrap();
        path
    }

    #[test]
    fn rollback_moves_files_back_and_removes_published_ones() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let moved = temp_file(root, "a.jpg");
        let published = temp_file(root, "b.jpg");
        let target = root.join("目标");
        fs::create_dir_all(&target).unwrap();
        let destination = target.join("a.jpg");
        fs::rename(&moved, &destination).unwrap();

        let mut journal = FileJournal::armed();
        let created = root.join("新建").join("深层");
        journal.create_dir_all(&created).unwrap();
        journal.renamed(&moved, &destination);
        journal.published(&published);
        journal.rollback();

        assert!(moved.exists(), "搬走的文件要回到原位");
        assert!(!destination.exists());
        assert!(!published.exists(), "发布的新文件要删掉");
        assert!(!created.exists());
        assert!(
            !root.join("新建").exists(),
            "本次新建的多级目录要按从深到浅删掉"
        );
        assert!(target.exists(), "没有记录过的目录不能被删");
    }

    #[test]
    fn disarmed_journal_keeps_everything() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp_file(temp.path(), "a.jpg");
        let destination = temp.path().join("b.jpg");
        fs::rename(&source, &destination).unwrap();

        {
            let mut journal = FileJournal::armed();
            journal.renamed(&source, &destination);
            journal.disarm();
        }
        assert!(destination.exists());
        assert!(!source.exists());
    }

    #[test]
    fn default_journal_never_touches_the_disk() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp_file(temp.path(), "a.jpg");
        let destination = temp.path().join("b.jpg");
        fs::rename(&source, &destination).unwrap();

        {
            let mut journal = FileJournal::default();
            journal.renamed(&source, &destination);
        }
        assert!(destination.exists());
    }

    #[test]
    fn rename_path_handles_case_only_changes() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("Art");
        fs::create_dir_all(&from).unwrap();
        let to = temp.path().join("art");
        rename_path(&from, &to, true).unwrap();
        assert!(to.is_dir());
        assert!(
            fs::read_dir(temp.path())
                .unwrap()
                .flatten()
                .all(|entry| entry.file_name() != "Art")
        );
    }
}

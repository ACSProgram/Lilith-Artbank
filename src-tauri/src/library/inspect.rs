//! 图库检查内核：一次目录遍历同时产出四份结论。
//!
//! 四条口径互相独立但共用同一次 `read_dir`：
//!
//! 1. **快速检查**：DB 行在原图目录里是否存在、`file_size` 是否一致。
//! 2. **完整校验**：在 1 成立的行上重算 BLAKE3 并与 `content_hash` 比较。
//! 3. **孤儿**：磁盘上有、但库里没有对应行（或同名哈希属于别的文件夹）。
//! 4. **缺失原图**：库里有行、磁盘上没有文件。
//!
//! 遍历按文件夹走 `read_dir`，不递归整库：`.artbank/` 里有十几 GB 的包文件，
//! 递归会把它们数进来。每个文件夹的行集与该目录的文件集在内存里比对，
//! 峰值是 O(最大单文件夹)，不是 O(全库)。
//!
//! 不持路径锁。全库完整校验是二十几分钟量级的读盘，长期持有根路径读锁会挡住
//! 导入与编辑；改为「容忍漂移」——扫描开始记时间水位，`modified_at` 落在水位之后
//! 的行跳过校验并单独计数。这与既有口径「不承诺任意时刻的全库快照一致」一致。

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    time::Instant,
};

use serde::Serialize;

use rusqlite::OptionalExtension;

use crate::{
    app::error::{AppError, AppResult},
    library::{BatchReporter, db::LibraryDb, paths::LibraryPaths},
};

/// 每份清单最多回传这么多条；39 万行全缺的极端情况下前端也显示不完。
pub const MAX_REPORT_ITEMS: usize = 5000;
/// 每这么多张检查一次取消标记。
const CANCEL_CHECK_EVERY: u64 = 256;
/// 每这么多张同步一次 `completed / total`；没必要每张都走一次锁。
const REPORT_EVERY: u64 = 64;
/// 校验哈希时的读缓冲。
const HASH_BUFFER: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InspectionMode {
    /// 只查存在性与大小。
    Fast,
    /// 在快速检查成立的行上重算 BLAKE3。
    Full,
}

impl InspectionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "快速检查",
            Self::Full => "完整校验",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingRow {
    pub image_id: i64,
    /// 图库内相对路径，例如 `人物/ab12….jpg`。
    pub path: String,
    pub expected_size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanFile {
    pub path: String,
    pub size: u64,
    /// 文件名里的哈希属于别的文件夹；为空表示库里根本没有这个哈希。
    pub other_folder: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SizeMismatch {
    pub image_id: i64,
    pub path: String,
    pub expected_size: u64,
    pub actual_size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HashMismatch {
    pub image_id: i64,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectionReport {
    pub task_id: String,
    pub library_id: String,
    pub mode: InspectionMode,
    /// 遍历了多少个文件夹。
    pub scanned_folders: u64,
    /// 比对过多少条 DB 行（不含孤儿文件，那些没有行）。
    pub scanned_rows: u64,
    /// 解析出多少个内容寻址文件。
    pub scanned_files: u64,
    pub missing_total: u64,
    pub orphan_total: u64,
    pub size_mismatch_total: u64,
    pub hash_mismatch_total: u64,
    pub non_content_files: u64,
    pub changed_during_scan: u64,
    pub hash_checked: u64,
    pub missing: Vec<MissingRow>,
    pub orphans: Vec<OrphanFile>,
    pub size_mismatch: Vec<SizeMismatch>,
    pub hash_mismatch: Vec<HashMismatch>,
    /// 清单被 `MAX_REPORT_ITEMS` 截断；界面提示只列出前 5000 条。
    pub truncated: bool,
    pub elapsed_ms: u64,
    pub finished_at: i64,
}

impl InspectionReport {
    /// 一条问题都没有：界面据此显示「没有发现问题」而不是四个 0。
    pub fn is_clean(&self) -> bool {
        self.missing_total == 0
            && self.orphan_total == 0
            && self.size_mismatch_total == 0
            && self.hash_mismatch_total == 0
    }

    pub fn orphan_paths(&self) -> impl Iterator<Item = &str> {
        self.orphans.iter().map(|orphan| orphan.path.as_str())
    }
}

/// 检查范围：`None` 表示整个图库的全部普通文件夹。
#[derive(Debug, Clone, Default)]
pub struct InspectScope {
    pub folder_ids: Option<Vec<i64>>,
}

struct Progress<'a> {
    reporter: &'a dyn BatchReporter,
    total: u64,
    done: u64,
    cancelled: bool,
}

impl<'a> Progress<'a> {
    fn new(reporter: &'a dyn BatchReporter, total: u64) -> Self {
        if let Some(meter) = reporter.meter() {
            meter.set_total_units(total);
        }
        reporter.report(0, total as usize);
        Self {
            reporter,
            total,
            done: 0,
            cancelled: false,
        }
    }

    /// 分母是范围内的行数，孤儿文件没有行却也要处理，所以 `done` 可能超过 `total`。
    /// 报给界面的值一律夹到分母内：进度条不回头，「已完成 k / n」也不会出现 k > n。
    fn report(&self) {
        self.reporter
            .report(self.done.min(self.total) as usize, self.total as usize);
    }

    fn tick(&mut self) {
        self.done += 1;
        if let Some(meter) = self.reporter.meter() {
            meter.advance_units(1);
        }
        if self.done.is_multiple_of(REPORT_EVERY) {
            self.report();
        }
        if self.done.is_multiple_of(CANCEL_CHECK_EVERY) && self.reporter.cancelled() {
            self.cancelled = true;
        }
    }

    fn finish(&mut self) {
        self.report();
    }

    fn should_stop(&self) -> bool {
        self.cancelled
    }
}

/// 单条 DB 行的检查输入。
struct RowSnapshot {
    id: i64,
    content_hash: String,
    file_ext: String,
    file_size: u64,
    modified_at: i64,
}

/// 跑一次检查。`root` 是图库根目录；进度按**张数**推进，与压缩同一口径。
pub fn inspect(
    database: &LibraryDb,
    root: &Path,
    task_id: &str,
    library_id: &str,
    mode: InspectionMode,
    scope: &InspectScope,
    reporter: &dyn BatchReporter,
) -> AppResult<InspectionReport> {
    let started = Instant::now();
    // 时间水位：`modified_at` 不早于它的行在扫描期间被动过，跳过哈希校验。
    let watermark = crate::library::service::now_ms();

    reporter.message("正在准备…");
    let folders = load_folders(database, scope)?;
    if folders.is_empty() {
        return Err(AppError::message("当前范围没有可检查的文件夹"));
    }
    let total_rows = count_rows(database, scope)?;
    reporter.message("");
    let mut progress = Progress::new(reporter, total_rows);

    let paths = LibraryPaths::new(root);
    let mut report = InspectionReport {
        task_id: task_id.to_owned(),
        library_id: library_id.to_owned(),
        mode,
        scanned_folders: 0,
        scanned_rows: 0,
        scanned_files: 0,
        missing_total: 0,
        orphan_total: 0,
        size_mismatch_total: 0,
        hash_mismatch_total: 0,
        non_content_files: 0,
        changed_during_scan: 0,
        hash_checked: 0,
        missing: Vec::new(),
        orphans: Vec::new(),
        size_mismatch: Vec::new(),
        hash_mismatch: Vec::new(),
        truncated: false,
        elapsed_ms: 0,
        finished_at: 0,
    };

    // 全局哈希 → 所在文件夹路径；用来区分「孤儿」与「这条哈希属于别的文件夹」。
    // 不预先载入全库哈希表：那是 O(全库) 内存。孤儿本来就少，逐个反查就够了。

    for (folder_id, folder_path) in &folders {
        if progress.should_stop() {
            break;
        }
        report.scanned_folders += 1;

        let rows = load_rows(database, *folder_id)?;
        let mut by_hash: HashMap<String, RowSnapshot> = HashMap::with_capacity(rows.len());
        for row in rows {
            by_hash.insert(row.content_hash.clone(), row);
        }

        let directory = paths.folder_dir(folder_path);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 文件夹被外部删掉：这一批行全部算缺失。
                for row in by_hash.values() {
                    report.scanned_rows += 1;
                    push_limited(
                        &mut report.missing,
                        &mut report.missing_total,
                        &mut report.truncated,
                        MissingRow {
                            image_id: row.id,
                            path: relative_display(folder_path, &row.content_hash, &row.file_ext),
                            expected_size: row.file_size,
                        },
                    );
                    progress.tick();
                }
                continue;
            }
            Err(error) => {
                return Err(AppError::message(format!(
                    "无法读取文件夹「{}」：{error}",
                    display_folder(folder_path)
                )));
            }
        };

        let mut seen: HashSet<i64> = HashSet::with_capacity(by_hash.len());
        let mut next = entries;
        loop {
            if progress.should_stop() {
                break;
            }
            let Some(entry) = next.next() else { break };
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some((hash, ext)) = parse_content_file_name(&name) else {
                // 不是本应用写的文件（`desktop.ini`、用户手工放进来的东西）：只计数。
                report.non_content_files += 1;
                continue;
            };
            report.scanned_files += 1;

            let Some(row) = by_hash.get(hash) else {
                let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                push_limited(
                    &mut report.orphans,
                    &mut report.orphan_total,
                    &mut report.truncated,
                    OrphanFile {
                        path: join_relative(folder_path, &name),
                        size,
                        other_folder: owner_folder(database, hash)?
                            .filter(|owner| owner != folder_path),
                    },
                );
                progress.tick();
                continue;
            };

            let actual = match entry.metadata() {
                Ok(meta) => meta.len(),
                Err(_) => row.file_size,
            };
            report.scanned_rows += 1;
            seen.insert(row.id);

            if actual != row.file_size || !ext.eq_ignore_ascii_case(&row.file_ext) {
                push_limited(
                    &mut report.size_mismatch,
                    &mut report.size_mismatch_total,
                    &mut report.truncated,
                    SizeMismatch {
                        image_id: row.id,
                        path: join_relative(folder_path, &name),
                        expected_size: row.file_size,
                        actual_size: actual,
                    },
                );
                progress.tick();
                continue;
            }

            if mode == InspectionMode::Full {
                if row.modified_at >= watermark {
                    // 扫描期间被编辑过（或刚好在开始前一刻）：不拿旧哈希判它的对错。
                    report.changed_during_scan += 1;
                } else {
                    report.hash_checked += 1;
                    match hash_file(&entry.path()) {
                        Ok(digest) => {
                            if !digest.eq_ignore_ascii_case(hash) {
                                push_limited(
                                    &mut report.hash_mismatch,
                                    &mut report.hash_mismatch_total,
                                    &mut report.truncated,
                                    HashMismatch {
                                        image_id: row.id,
                                        path: join_relative(folder_path, &name),
                                    },
                                );
                            }
                        }
                        Err(error) => {
                            log::warn!("读取「{name}」计算哈希失败：{error}");
                            push_limited(
                                &mut report.hash_mismatch,
                                &mut report.hash_mismatch_total,
                                &mut report.truncated,
                                HashMismatch {
                                    image_id: row.id,
                                    path: join_relative(folder_path, &name),
                                },
                            );
                        }
                    }
                }
            }
            progress.tick();
        }

        // 这个文件夹里没有对应文件的行就是缺失原图。
        for row in by_hash.values() {
            if seen.contains(&row.id) {
                continue;
            }
            if progress.should_stop() {
                // 取消时也要把当前文件夹剩下的行记成缺失吗？不：没扫到的部分不算结论。
                break;
            }
            report.scanned_rows += 1;
            push_limited(
                &mut report.missing,
                &mut report.missing_total,
                &mut report.truncated,
                MissingRow {
                    image_id: row.id,
                    path: relative_display(folder_path, &row.content_hash, &row.file_ext),
                    expected_size: row.file_size,
                },
            );
            progress.tick();
        }
    }

    progress.finish();
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report.finished_at = crate::library::service::now_ms();
    Ok(report)
}

fn push_limited<T>(items: &mut Vec<T>, total: &mut u64, truncated: &mut bool, item: T) {
    *total += 1;
    if items.len() < MAX_REPORT_ITEMS {
        items.push(item);
    } else {
        *truncated = true;
    }
}

fn display_folder(path: &str) -> &str {
    if path.is_empty() { "根目录" } else { path }
}

pub fn relative_display(folder_path: &str, content_hash: &str, file_ext: &str) -> String {
    join_relative(folder_path, &format!("{content_hash}.{file_ext}"))
}

pub fn join_relative(folder_path: &str, name: &str) -> String {
    if folder_path.is_empty() {
        name.to_owned()
    } else {
        format!("{folder_path}/{name}")
    }
}

/// 文件名必须是 `<64 位小写十六进制>.<扩展名>`。
pub fn parse_content_file_name(name: &str) -> Option<(&str, &str)> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.len() != 64 || ext.is_empty() || ext.len() > 16 {
        return None;
    }
    if !stem
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some((stem, ext))
}

/// 分条流式计算 BLAKE3，几十 MB 的原图也不整个读进内存。
fn hash_file(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(HASH_BUFFER, file);
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; HASH_BUFFER];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn load_folders(database: &LibraryDb, scope: &InspectScope) -> AppResult<Vec<(i64, String)>> {
    database.with_reader(|connection| {
        let folders = match &scope.folder_ids {
            None => {
                let mut statement = connection.prepare(
                    "SELECT id, path FROM folders
                     WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL",
                )?;
                statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<(i64, String)>>>()?
            }
            Some(ids) => {
                let mut folders = Vec::new();
                for chunk in ids.chunks(400) {
                    let marks = crate::library::placeholders(chunk.len());
                    let mut statement = connection.prepare(&format!(
                        "SELECT id, path FROM folders
                         WHERE kind = 'normal' AND trashed_at IS NULL AND purge_operation_id IS NULL
                           AND id IN ({marks}) AND id > 0"
                    ))?;
                    folders.extend(
                        statement
                            .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                                Ok((row.get(0)?, row.get(1)?))
                            })?
                            .collect::<rusqlite::Result<Vec<(i64, String)>>>()?,
                    );
                }
                folders
            }
        };
        Ok(folders)
    })
}

fn count_rows(database: &LibraryDb, scope: &InspectScope) -> AppResult<u64> {
    database.with_reader(|connection| {
        let count: i64 = match &scope.folder_ids {
            None => connection.query_row("SELECT count(*) FROM images", [], |row| row.get(0))?,
            Some(ids) => {
                let mut total = 0_i64;
                for chunk in ids.chunks(400) {
                    let marks = crate::library::placeholders(chunk.len());
                    // 与 `load_folders` 同口径：临时文件夹、回收站与待释放文件夹不是图库的一部分。
                    let mut statement = connection.prepare(&format!(
                        "SELECT count(*) FROM images
                         WHERE folder_id IN ({marks})
                           AND folder_id IN (
                               SELECT id FROM folders
                               WHERE kind = 'normal' AND trashed_at IS NULL
                                 AND purge_operation_id IS NULL
                           )"
                    ))?;
                    total += statement
                        .query_row(rusqlite::params_from_iter(chunk.iter()), |row| {
                            row.get::<_, i64>(0)
                        })?;
                }
                total
            }
        };
        Ok(count.max(0) as u64)
    })
}

/// 这个哈希登记在哪个文件夹；库里没有这个哈希时返回 `None`。
///
/// 只在发现孤儿时调用一次：`content_hash` 有唯一约束，最多命中一行。
fn owner_folder(database: &LibraryDb, content_hash: &str) -> AppResult<Option<String>> {
    database.with_reader(|connection| {
        Ok(connection
            .query_row(
                "SELECT folders.path FROM images JOIN folders ON folders.id = images.folder_id
                 WHERE images.content_hash = ?1",
                [content_hash],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    })
}

fn load_rows(database: &LibraryDb, folder_id: i64) -> AppResult<Vec<RowSnapshot>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, content_hash, file_ext, file_size, modified_at
             FROM images WHERE folder_id = ?1",
        )?;
        let rows = statement
            .query_map([folder_id], |row| {
                Ok(RowSnapshot {
                    id: row.get(0)?,
                    content_hash: row.get(1)?,
                    file_ext: row.get(2)?,
                    file_size: row.get::<_, i64>(3)?.max(0) as u64,
                    modified_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

/// 报告导出成 CSV：几万行不塞进 IPC 再塞回来，后端直接写文件。
///
/// 只导出一组；`kind` 为空时四组全导。
///
/// 列头固定，`kind` 列本来就能在一张表里区分四类问题；按组导出只是少写几行，
/// 不必让前端先接几万行再自己拼一遍 CSV。
pub fn report_csv_group(report: &InspectionReport, kind: Option<&str>) -> String {
    let wanted = |name: &str| kind.is_none_or(|kind| kind == name);
    let mut out = String::with_capacity(4096);
    out.push_str("kind,path,image_id,expected_size,actual_size,detail\n");
    for item in report.missing.iter().filter(|_| wanted("missing")) {
        out.push_str(&format!(
            "missing,{},{},{},,\n",
            csv_field(&item.path),
            item.image_id,
            item.expected_size
        ));
    }
    for item in report.orphans.iter().filter(|_| wanted("orphan")) {
        out.push_str(&format!(
            "orphan,{},,{},,{}\n",
            csv_field(&item.path),
            item.size,
            csv_field(item.other_folder.as_deref().unwrap_or(""))
        ));
    }
    for item in report
        .size_mismatch
        .iter()
        .filter(|_| wanted("sizeMismatch"))
    {
        out.push_str(&format!(
            "sizeMismatch,{},{},{},{},\n",
            csv_field(&item.path),
            item.image_id,
            item.expected_size,
            item.actual_size
        ));
    }
    for item in report
        .hash_mismatch
        .iter()
        .filter(|_| wanted("hashMismatch"))
    {
        out.push_str(&format!(
            "hashMismatch,{},{},,,\n",
            csv_field(&item.path),
            item.image_id
        ));
    }
    out
}

fn csv_field(value: &str) -> String {
    if value.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// 图库内相对路径拼成绝对路径；拒绝跳出图库根目录的相对路径。
pub fn absolute_path(root: &Path, relative: &str) -> AppResult<PathBuf> {
    let mut path = root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(AppError::message(format!("路径无效：{relative}")));
        }
        path.push(segment);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{NoopReporter, service};

    struct Fixture {
        root: PathBuf,
        session: service::LibrarySession,
        folder: i64,
        _temp: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("library");
            let (session, _) = service::create(&root, "检查测试", 1).unwrap();
            // 根文件夹由 schema 初始化建好（id=1）；测试直接用它的 id。
            let folder = session
                .db
                .with_reader(|connection| {
                    Ok(connection.query_row(
                        "SELECT id FROM folders WHERE path = ''",
                        [],
                        |row| row.get::<_, i64>(0),
                    )?)
                })
                .unwrap();
            Self {
                root,
                session,
                folder,
                _temp: temp,
            }
        }

        fn db(&self) -> &LibraryDb {
            &self.session.db
        }

        /// 写一张原图并插入行；返回图片 id。
        fn add_image(&self, hash: &str, ext: &str, bytes: &[u8]) -> i64 {
            std::fs::write(self.root.join(format!("{hash}.{ext}")), bytes).unwrap();
            self.db()
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, ?2, ?3, ?4, 4, 4, 'jpeg', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        rusqlite::params![hash, ext, self.folder, bytes.len() as i64],
                    )?;
                    Ok(connection.last_insert_rowid())
                })
                .unwrap()
        }
    }

    fn run(fixture: &Fixture, mode: InspectionMode) -> InspectionReport {
        inspect(
            fixture.db(),
            &fixture.root,
            "t1",
            "lib",
            mode,
            &InspectScope::default(),
            &NoopReporter,
        )
        .unwrap()
    }

    #[test]
    fn clean_library_reports_nothing() {
        let fixture = Fixture::new();
        let hash = blake3::hash(b"hello").to_hex().to_string();
        fixture.add_image(&hash, "jpg", b"hello");
        let report = run(&fixture, InspectionMode::Fast);
        assert_eq!(report.scanned_rows, 1);
        assert_eq!(report.scanned_files, 1);
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn missing_file_is_reported_with_relative_path() {
        let fixture = Fixture::new();
        let hash = blake3::hash(b"gone").to_hex().to_string();
        let id = fixture.add_image(&hash, "jpg", b"gone");
        std::fs::remove_file(fixture.root.join(format!("{hash}.jpg"))).unwrap();
        let report = run(&fixture, InspectionMode::Fast);
        assert_eq!(report.missing_total, 1);
        assert_eq!(report.missing[0].image_id, id);
        assert_eq!(report.missing[0].path, format!("{hash}.jpg"));
    }

    #[test]
    fn unknown_hash_is_an_orphan_and_non_content_files_only_count() {
        let fixture = Fixture::new();
        let orphan = "0".repeat(64);
        std::fs::write(fixture.root.join(format!("{orphan}.jpg")), b"x").unwrap();
        std::fs::write(fixture.root.join("desktop.ini"), b"x").unwrap();
        let report = run(&fixture, InspectionMode::Fast);
        assert_eq!(report.orphan_total, 1);
        assert_eq!(report.orphans[0].path, format!("{orphan}.jpg"));
        assert_eq!(report.non_content_files, 1);
        assert!(report.missing.is_empty());
    }

    #[test]
    fn same_hash_in_another_folder_is_an_orphan() {
        let fixture = Fixture::new();
        let hash = blake3::hash(b"dup").to_hex().to_string();
        fixture.add_image(&hash, "jpg", b"dup");
        std::fs::create_dir_all(fixture.root.join("人物")).unwrap();
        std::fs::write(
            fixture.root.join("人物").join(format!("{hash}.jpg")),
            b"dup",
        )
        .unwrap();
        fixture
            .db()
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO folders(path, created_at, sort_index, kind)
                     VALUES ('人物', 1, 0, 'normal')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let report = run(&fixture, InspectionMode::Fast);
        assert_eq!(report.orphan_total, 1);
        assert_eq!(report.orphans[0].other_folder.as_deref(), Some(""));
        assert_eq!(report.orphans[0].path, format!("人物/{hash}.jpg"));
    }

    #[test]
    fn size_mismatch_is_separate_from_hash_mismatch() {
        let fixture = Fixture::new();
        let bytes = b"abcdef";
        let hash = blake3::hash(bytes).to_hex().to_string();
        fixture.add_image(&hash, "jpg", bytes);
        std::fs::write(fixture.root.join(format!("{hash}.jpg")), b"abc").unwrap();
        let fast = run(&fixture, InspectionMode::Fast);
        assert_eq!(fast.size_mismatch_total, 1);
        assert_eq!(fast.size_mismatch[0].expected_size, 6);
        assert_eq!(fast.size_mismatch[0].actual_size, 3);
        assert_eq!(fast.hash_mismatch_total, 0);
    }

    #[test]
    fn full_check_finds_a_tampered_byte() {
        let fixture = Fixture::new();
        let bytes = b"content";
        let hash = blake3::hash(bytes).to_hex().to_string();
        fixture.add_image(&hash, "jpg", bytes);
        // 同样大小、内容不同：快速检查看不出来，完整校验能发现。
        std::fs::write(fixture.root.join(format!("{hash}.jpg")), b"contenT").unwrap();
        assert_eq!(run(&fixture, InspectionMode::Fast).hash_mismatch_total, 0);
        let full = run(&fixture, InspectionMode::Full);
        assert_eq!(full.hash_checked, 1);
        assert_eq!(full.hash_mismatch_total, 1);
    }

    #[test]
    fn rows_changed_during_scan_are_skipped_for_hashing() {
        let fixture = Fixture::new();
        let bytes = b"drift";
        let hash = blake3::hash(bytes).to_hex().to_string();
        let id = fixture.add_image(&hash, "jpg", bytes);
        // 把 modified_at 推到未来：等价于「扫描期间被改过」。
        fixture
            .db()
            .with_writer(|connection| {
                connection.execute(
                    "UPDATE images SET modified_at = ?1 WHERE id = ?2",
                    rusqlite::params![i64::MAX / 2, id],
                )?;
                Ok(())
            })
            .unwrap();
        std::fs::write(fixture.root.join(format!("{hash}.jpg")), b"drifT").unwrap();
        let report = run(&fixture, InspectionMode::Full);
        assert_eq!(report.changed_during_scan, 1);
        assert_eq!(report.hash_checked, 0);
        assert_eq!(report.hash_mismatch_total, 0);
    }

    #[test]
    fn report_lists_are_capped() {
        let mut items: Vec<u32> = Vec::new();
        let mut total = 0;
        let mut truncated = false;
        for value in 0..(MAX_REPORT_ITEMS + 3) {
            push_limited(&mut items, &mut total, &mut truncated, value as u32);
        }
        assert_eq!(items.len(), MAX_REPORT_ITEMS);
        assert_eq!(total as usize, MAX_REPORT_ITEMS + 3);
        assert!(truncated);
    }

    #[test]
    fn cancellation_keeps_partial_report() {
        struct CancelAfter(std::sync::atomic::AtomicU64);
        impl BatchReporter for CancelAfter {
            fn report(&self, _completed: usize, _total: usize) {}
            fn cancelled(&self) -> bool {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0
            }
        }

        let fixture = Fixture::new();
        let hash = blake3::hash(b"a").to_hex().to_string();
        fixture.add_image(&hash, "jpg", b"a");
        let reporter = CancelAfter(std::sync::atomic::AtomicU64::new(0));
        let report = inspect(
            fixture.db(),
            &fixture.root,
            "t1",
            "lib",
            InspectionMode::Fast,
            &InspectScope::default(),
            &reporter,
        )
        .unwrap();
        // 取消后仍返回报告，且已扫描的部分结果保留。
        assert!(report.scanned_rows <= 1);
    }

    #[test]
    fn csv_escapes_commas_and_quotes() {
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_field("人物/a.jpg"), "人物/a.jpg");
    }

    #[test]
    fn rejects_paths_escaping_the_library() {
        let root = Path::new("C:/lib");
        assert!(absolute_path(root, "../外面/a.jpg").is_err());
        assert_eq!(
            absolute_path(root, "人物/a.jpg").unwrap(),
            root.join("人物").join("a.jpg")
        );
    }

    #[test]
    fn parse_content_file_name_is_strict() {
        let hash = "a".repeat(64);
        assert!(parse_content_file_name(&format!("{hash}.jpg")).is_some());
        assert!(parse_content_file_name(&format!("{}.jpg", "A".repeat(64))).is_none());
        assert!(parse_content_file_name("desktop.ini").is_none());
        assert!(parse_content_file_name(&hash).is_none());
    }
}

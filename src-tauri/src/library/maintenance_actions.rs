//! 检查发现问题之后的处理动作：孤儿移入回收站与导出到目录。
//!
//! 两者都不改变图库内容：孤儿本来就不在库里，导出只读原图。孤儿移入仍然写一条
//! 操作记录（`orphanMove`），因为它是「搬走文件」这种不可见但可逆的动作，
//! 撤销必须能把文件移回原位。

use std::{
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    app::error::{AppError, AppResult},
    library::{
        BatchReporter,
        db::LibraryDb,
        fs_journal::{self, FileJournal},
        history::{self, HistoryKind},
        inspect::{InspectionReport, absolute_path},
        paths::{LibraryPaths, RECYCLE_DIR},
    },
};

/// 回收站里专门放孤儿文件的目录，与图片回收站（按状态标记）分开。
pub const ORPHAN_DIR: &str = "orphans";
/// 导出复制用的缓冲：8 MB 一档，几百 MB 的原图也不会整个读进内存。
const COPY_BUFFER: usize = 8 * 1024 * 1024;
/// 每这么多张检查一次取消标记。
const CANCEL_CHECK_EVERY: u64 = 16;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanFailure {
    /// 图库内相对路径，与检查报告里的路径同形。
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanMoveReport {
    pub moved: u64,
    /// 实际搬走的相对路径；取消时只包含已搬走的那部分，界面按它摘报告条目。
    pub moved_paths: Vec<String>,
    /// 搬失败的文件与原因；不中断整次操作。
    pub failed: Vec<OrphanFailure>,
    pub operation_id: Option<i64>,
}

/// 把检查报告里的孤儿移入 `recycle/orphans/`。
///
/// 只接受**当次报告里列出的路径**：任意路径列表会让这条命令变成「按用户给的路径
/// 随便搬文件」的口子。重名追加 `-1`、`-2`；跨卷时退化为复制 + 删源。
/// 整次操作写**一条**记录，撤销一次退回整次清理。
///
/// 顺序是「先搬文件，再用短事务记账」：单写连接不会被几百 GB 的搬移长期占住，
/// 导入 flush 与元数据写入不必等整次清理。代价是进程在两步之间被强杀时，文件已经
/// 在回收站里而没有记录——这种情况下文件仍可手工找回，且不影响库内一致性；
/// 反过来（先记账后搬文件）则会出现记录指向没搬走的文件，撤销会失败。
pub fn move_orphans(
    database: &LibraryDb,
    root: &Path,
    report: &InspectionReport,
    paths: &[String],
    reporter: &dyn BatchReporter,
) -> AppResult<OrphanMoveReport> {
    if paths.is_empty() {
        return Err(AppError::message("没有要移走的孤儿文件"));
    }
    let known: std::collections::HashSet<&str> = report.orphan_paths().collect();
    let mut sources: Vec<(String, PathBuf)> = Vec::with_capacity(paths.len());
    for path in paths {
        if !known.contains(path.as_str()) {
            return Err(AppError::message(format!(
                "「{path}」不在这次检查的孤儿清单里，请重新扫描"
            )));
        }
        sources.push((path.clone(), absolute_path(root, path)?));
    }

    let target_dir = root.join(RECYCLE_DIR).join(ORPHAN_DIR);
    // 记账失败时把已经搬走的文件搬回原位；只有记录写成功才 `disarm()`。
    let mut journal = FileJournal::armed();
    journal.create_dir_all(&target_dir)?;
    reporter.report(0, sources.len());
    let mut failed: Vec<OrphanFailure> = Vec::new();
    let mut moved_pairs: Vec<(String, String)> = Vec::with_capacity(sources.len());
    let mut moved_paths: Vec<String> = Vec::with_capacity(sources.len());
    let mut done = 0_u64;
    let mut cancelled = false;

    for (relative, source) in &sources {
        if reporter.cancelled() {
            cancelled = true;
            break;
        }
        done += 1;
        if done.is_multiple_of(CANCEL_CHECK_EVERY) {
            reporter.report(done as usize, sources.len());
        }
        let name = relative.rsplit('/').next().unwrap_or(relative);
        let Some(destination) = unique_destination(&target_dir, name) else {
            failed.push(OrphanFailure {
                path: relative.clone(),
                reason: "无法生成不重名的目标文件名".to_owned(),
            });
            continue;
        };
        match fs_journal::move_file(source, &destination) {
            Ok(()) => {
                journal.renamed(source, &destination);
                moved_paths.push(relative.clone());
                moved_pairs.push((
                    source.to_string_lossy().into_owned(),
                    destination.to_string_lossy().into_owned(),
                ));
            }
            Err(error) => failed.push(OrphanFailure {
                path: relative.clone(),
                reason: error.to_string(),
            }),
        }
    }
    reporter.report(done as usize, sources.len());

    let label = format!("清理孤儿文件 · {} 个", sources.len());
    let operation_id = if moved_pairs.is_empty() {
        None
    } else {
        let ((), operation_id) = history::write_with_id(
            database,
            history::NewOperation::new(HistoryKind::OrphanMove, label),
            |_, draft| {
                draft.json(&moved_pairs)?;
                draft.affected(moved_pairs.len() as u64);
                Ok(())
            },
        )?;
        operation_id
    };
    journal.disarm();

    if cancelled {
        log::info!("清理孤儿在下标 {done} 处取消，已搬走的保留在回收站");
    }
    Ok(OrphanMoveReport {
        moved: moved_pairs.len() as u64,
        moved_paths,
        failed,
        operation_id,
    })
}

fn unique_destination(directory: &Path, name: &str) -> Option<PathBuf> {
    let candidate = directory.join(name);
    if !candidate.exists() {
        return Some(candidate);
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem, format!(".{ext}")),
        None => (name, String::new()),
    };
    for index in 1..=9_999 {
        let candidate = directory.join(format!("{stem}-{index}{ext}"));
        if !candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// 同卷用 rename；跨卷（`EXDEV`）退化为复制 + 删源。

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub copied: u64,
    pub skipped: u64,
    pub failed: Vec<String>,
    pub bytes: u64,
    pub manifest: bool,
}

struct ExportRow {
    content_hash: String,
    file_ext: String,
    folder_path: String,
    width: u32,
    height: u32,
    score: i64,
    description: String,
}

/// 把原图复制到目标目录，保留库里的文件名（`<content_hash>.<ext>`）。
///
/// 目标已存在同名文件时**跳过并计数**，不覆盖、不改名：用户的目录里可能已经有
/// 同一次导出的结果，覆盖会悄悄吃掉别的东西。
///
/// `include_manifest` 时写 `manifest.csv`，只列目标目录里确实存在的文件（复制成功或
/// 本来就在）；复制失败的条目只进失败清单，不写进清单文件。
pub fn export(
    database: &LibraryDb,
    root: &Path,
    image_ids: &[i64],
    target_dir: &Path,
    include_manifest: bool,
    reporter: &dyn BatchReporter,
) -> AppResult<ExportReport> {
    if image_ids.is_empty() {
        return Err(AppError::message("当前范围没有图片"));
    }
    if target_dir.as_os_str().is_empty() {
        return Err(AppError::message("请选择导出目录"));
    }
    fs::create_dir_all(target_dir)?;
    let paths = LibraryPaths::new(root);

    let mut report = ExportReport {
        copied: 0,
        skipped: 0,
        failed: Vec::new(),
        bytes: 0,
        manifest: include_manifest,
    };
    let mut manifest: Vec<ExportRow> = Vec::new();
    reporter.report(0, image_ids.len());

    let mut done = 0_usize;
    let mut cancelled = false;
    for chunk in image_ids.chunks(400) {
        if cancelled {
            break;
        }
        let rows = load_rows(database, chunk)?;
        for row in rows {
            if reporter.cancelled() {
                cancelled = true;
                break;
            }
            done += 1;
            let name = format!("{}.{}", row.content_hash, row.file_ext);
            let source = paths.content_file(&row.folder_path, &name);
            let destination = target_dir.join(&name);
            if destination.exists() {
                report.skipped += 1;
                if include_manifest {
                    manifest.push(row);
                }
            } else {
                match copy_stream(&source, &destination) {
                    Ok(bytes) => {
                        report.copied += 1;
                        report.bytes += bytes;
                        if include_manifest {
                            manifest.push(row);
                        }
                    }
                    Err(error) => {
                        report.failed.push(format!("{name}：{error}"));
                        let _ = fs::remove_file(&destination);
                    }
                }
            }
            reporter.report(done, image_ids.len());
            if done.is_multiple_of(8) {
                reporter.bytes(report.bytes, 0);
            }
        }
    }

    if cancelled {
        log::info!("导出在下标 {done} 处取消，已复制的文件保留在目标目录");
    }
    if include_manifest && !manifest.is_empty() {
        write_manifest(&target_dir.join("manifest.csv"), &manifest)?;
    }
    Ok(report)
}

fn load_rows(database: &LibraryDb, ids: &[i64]) -> AppResult<Vec<ExportRow>> {
    let marks = crate::library::placeholders(ids.len());
    database.with_reader(|connection| {
        let mut statement = connection.prepare(&format!(
            "SELECT images.content_hash, images.file_ext, folders.path,
                    images.width, images.height, images.score, images.description
             FROM images JOIN folders ON folders.id = images.folder_id
             WHERE images.id IN ({marks})
             ORDER BY images.id"
        ))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok(ExportRow {
                    content_hash: row.get(0)?,
                    file_ext: row.get(1)?,
                    folder_path: row.get(2)?,
                    width: row.get::<_, i64>(3)?.max(0) as u32,
                    height: row.get::<_, i64>(4)?.max(0) as u32,
                    score: row.get(5)?,
                    description: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

fn copy_stream(source: &Path, destination: &Path) -> io::Result<u64> {
    let mut input = File::open(source)?;
    let output = File::create(destination)?;
    let mut writer = BufWriter::with_capacity(COPY_BUFFER, output);
    let bytes = io::copy(&mut input, &mut writer)?;
    writer.flush()?;
    Ok(bytes)
}

/// `manifest.csv`：能被 Excel 与脚本同时吃，所以不用 JSON。
///
/// 开头写 UTF-8 BOM：没有它 Excel 会把中文描述读成乱码。
fn write_manifest(path: &Path, rows: &[ExportRow]) -> AppResult<()> {
    let mut out = String::with_capacity(rows.len() * 96);
    out.push('\u{feff}');
    out.push_str("content_hash,file_ext,width,height,folder_path,score,description\n");
    for row in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            row.content_hash,
            csv_field(&row.file_ext),
            row.width,
            row.height,
            csv_field(&row.folder_path),
            row.score,
            csv_field(&row.description)
        ));
    }
    fs::write(path, out)?;
    Ok(())
}

fn csv_field(value: &str) -> String {
    if value.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// 撤销/重做孤儿清理时移动文件的共用实现。
pub(crate) fn move_pairs(pairs: &[(String, String)]) -> Vec<String> {
    let mut skipped = Vec::new();
    for (from, to) in pairs {
        let from = Path::new(from);
        let to = Path::new(to);
        if !from.is_file() {
            skipped.push(format!("{} 已不在原位置", from.display()));
            continue;
        }
        if to.exists() {
            skipped.push(format!("{} 已被占用", to.display()));
            continue;
        }
        if let Some(parent) = to.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            skipped.push(format!("{}：{error}", to.display()));
            continue;
        }
        if let Err(error) = fs_journal::move_file(from, to) {
            skipped.push(format!("{}：{error}", to.display()));
        }
    }
    skipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{
        NoopReporter,
        inspect::{InspectScope, InspectionMode, OrphanFile},
        service,
    };

    struct Fixture {
        root: PathBuf,
        session: service::LibrarySession,
        _temp: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("library");
            let (session, _) = service::create(&root, "维护动作测试", 1).unwrap();

            Self {
                root,
                session,
                _temp: temp,
            }
        }

        fn db(&self) -> &LibraryDb {
            &self.session.db
        }

        fn write_orphan(&self, name: &str) -> String {
            let path = self.root.join(name);
            fs::write(&path, b"orphan").unwrap();
            name.to_owned()
        }

        /// 造一份只含指定孤儿的报告，省掉一次真实扫描。
        fn report(&self, paths: &[&str]) -> InspectionReport {
            let mut report = InspectionReport {
                task_id: "t1".into(),
                library_id: "lib".into(),
                mode: InspectionMode::Fast,
                scanned_folders: 1,
                scanned_rows: 0,
                scanned_files: paths.len() as u64,
                missing_total: 0,
                orphan_total: paths.len() as u64,
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
            for path in paths {
                report.orphans.push(OrphanFile {
                    path: (*path).to_owned(),
                    size: 6,
                    other_folder: None,
                });
            }
            report
        }
    }

    #[test]
    fn moves_orphans_into_the_recycle_folder() {
        let fixture = Fixture::new();
        let name = fixture.write_orphan("dead.jpg");
        let report = fixture.report(&[&name]);
        let result = move_orphans(
            fixture.db(),
            &fixture.root,
            &report,
            std::slice::from_ref(&name),
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(result.moved, 1);
        assert!(result.operation_id.is_some());
        assert!(!fixture.root.join(&name).exists());
        assert!(
            fixture
                .root
                .join(RECYCLE_DIR)
                .join(ORPHAN_DIR)
                .join(&name)
                .exists()
        );
    }

    #[test]
    fn refuses_paths_outside_the_report() {
        let fixture = Fixture::new();
        let report = fixture.report(&[]);
        let error = move_orphans(
            fixture.db(),
            &fixture.root,
            &report,
            &["c:/windows/system32/x.jpg".into()],
            &NoopReporter,
        )
        .unwrap_err();
        assert!(error.to_string().contains("不在这次检查的孤儿清单里"));
    }

    #[test]
    fn duplicate_names_get_a_suffix() {
        let fixture = Fixture::new();
        let directory = fixture.root.join(RECYCLE_DIR).join(ORPHAN_DIR);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("dead.jpg"), b"old").unwrap();
        let name = fixture.write_orphan("dead.jpg");
        let report = fixture.report(&[&name]);
        move_orphans(fixture.db(), &fixture.root, &report, &[name], &NoopReporter).unwrap();
        assert!(directory.join("dead-1.jpg").exists());
    }

    #[test]
    fn export_copies_files_and_skips_existing() {
        let fixture = Fixture::new();
        let bytes = b"image-bytes";
        let hash = blake3::hash(bytes).to_hex().to_string();
        fs::write(fixture.root.join(format!("{hash}.jpg")), bytes).unwrap();
        let id = fixture
            .db()
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'jpg', 1, ?2, 8, 8, 'jpeg', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 3, '描述,带逗号', 1, 1)",
                    rusqlite::params![hash, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();

        let target = tempfile::tempdir().unwrap();
        let first = export(
            fixture.db(),
            &fixture.root,
            &[id],
            target.path(),
            true,
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(first.copied, 1);
        assert_eq!(first.skipped, 0);
        assert_eq!(first.bytes, bytes.len() as u64);
        assert!(target.path().join(format!("{hash}.jpg")).exists());
        let manifest = fs::read_to_string(target.path().join("manifest.csv")).unwrap();
        assert!(manifest.starts_with('\u{feff}'), "BOM 必须在最前面");
        assert!(manifest.contains("\"描述,带逗号\""), "{manifest}");

        let second = export(
            fixture.db(),
            &fixture.root,
            &[id],
            target.path(),
            false,
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(second.copied, 0);
        assert_eq!(second.skipped, 1);
    }

    #[test]
    fn move_pairs_reports_occupied_destinations() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("a.jpg");
        let to = temp.path().join("b.jpg");
        fs::write(&from, b"x").unwrap();
        fs::write(&to, b"y").unwrap();
        let skipped = move_pairs(&[(
            from.to_string_lossy().into_owned(),
            to.to_string_lossy().into_owned(),
        )]);
        assert_eq!(skipped.len(), 1);
        assert!(from.exists(), "跳过的条目要留在原位");
    }

    #[test]
    fn export_requires_a_target_directory() {
        let fixture = Fixture::new();
        let error = export(
            fixture.db(),
            &fixture.root,
            &[1],
            Path::new(""),
            false,
            &NoopReporter,
        )
        .unwrap_err();
        assert!(error.to_string().contains("导出目录"));
    }

    #[test]
    fn export_scope_scan_still_works_end_to_end() {
        let fixture = Fixture::new();
        let bytes = b"abc";
        let hash = blake3::hash(bytes).to_hex().to_string();
        fs::write(fixture.root.join(format!("{hash}.jpg")), bytes).unwrap();
        fixture
            .db()
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, 'jpg', 1, 3, 4, 4, 'jpeg', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    rusqlite::params![hash],
                )?;
                Ok(())
            })
            .unwrap();
        let report = crate::library::inspect::inspect(
            fixture.db(),
            &fixture.root,
            "t1",
            "lib",
            InspectionMode::Fast,
            &InspectScope::default(),
            &NoopReporter,
        )
        .unwrap();
        assert!(report.is_clean());
    }
}

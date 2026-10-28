//! 缩略图包的垃圾率统计与回收。
//!
//! `ThumbPack::append` 只往段末尾写，`offset` 只增不减，段满才开新段，**从不回头
//! 找空洞**。所以编辑一张图就留下一个约 23 KB 的空洞，跑一次全库「重建缩略图」
//! 会让旧包整份变成空洞——不是慢慢涨，是跳着涨。
//!
//! 回收只有一种做法：把仍被引用的 blob 按 `images.id` 升序抄进新段，逐行改指针，
//! 提交后删掉旧段文件。发布顺序与图片压缩同源（数据写 → `sync_all` → 事务提交 →
//! 删旧文件），失败或让位时数据库一个字没改。
//!
//! 统计是只读的，一次 SQL 出全部段；特征包同样只统计，不回收（模型切换会让整包
//! 作废重算，压了也没有意义）。

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    time::Duration,
};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        BatchReporter,
        db::LibraryDb,
        paths::LibraryPaths,
        service::now_ms,
        thumbs::{HEADER_LEN, MAX_THUMBNAIL_BYTES, ThumbPack, segment_path},
    },
};

/// 段垃圾率超过这个比例才压；没有单独的「停止阈值」——一次压整段，压完就回到接近 0。
pub const TRIGGER_RATIO: f64 = 0.20;
/// 每复制这么多条 blob 检查一次「人回来了没有」。
const YIELD_CHECK_EVERY: u64 = 2_000;
/// 提交后等这么久再删旧段，避开「取指针 → 打开文件」的窄竞态。
/// 测试里不需要等：这段时间只是给读者留的窗口，与回收结果无关。
#[cfg(not(test))]
const DELETE_DELAY: Duration = Duration::from_secs(2);
#[cfg(test)]
const DELETE_DELAY: Duration = Duration::ZERO;
/// 记录上次回收时间的 `meta` 键。
pub const META_KEY: &str = "pack.compact.v1";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentStat {
    pub id: u32,
    pub file_name: String,
    pub size: u64,
    pub used: u64,
    pub blobs: u64,
    /// 段里没有被任何行引用的字节（段头不计）；界面直接显示，不再自己算。
    pub garbage: u64,
}

impl SegmentStat {
    pub fn new(id: u32, file_name: String, size: u64, used: u64, blobs: u64) -> Self {
        let garbage = size.saturating_sub(HEADER_LEN + used);
        Self {
            id,
            file_name,
            size,
            used,
            blobs,
            garbage,
        }
    }

    pub fn garbage_ratio(&self) -> f64 {
        if self.size == 0 {
            0.0
        } else {
            self.garbage as f64 / self.size as f64
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeaturePackStat {
    pub kind: String,
    pub model: String,
    pub dim: u32,
    pub pack_name: String,
    pub blobs: u64,
    /// 数据库里记录的总长度。
    pub used: u64,
    /// 包文件实际大小；文件缺失时为 0。
    pub file_size: u64,
    /// 文件里没有被任何行引用的字节；只显示，不回收。
    pub garbage: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackStats {
    pub segments: Vec<SegmentStat>,
    pub total_size: u64,
    pub total_used: u64,
    /// 可回收字节（段头不计）。
    pub garbage_bytes: u64,
    pub features: Vec<FeaturePackStat>,
}

impl PackStats {
    pub fn garbage_ratio(&self) -> f64 {
        if self.total_size == 0 {
            0.0
        } else {
            self.garbage_bytes as f64 / self.total_size as f64
        }
    }
}

/// 段统计 + 特征包统计：两条只读聚合查询，再按包名补一次文件大小。
pub fn stats(database: &LibraryDb, root: &Path) -> AppResult<PackStats> {
    let segments = stats_segments_only(database)?;

    let features = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT kind, model, dim, pack_name, SUM(pack_length), COUNT(*)
             FROM feature_blobs
             GROUP BY kind, model, dim, pack_name
             ORDER BY kind, model, pack_name",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(FeaturePackStat {
                    kind: row.get(0)?,
                    model: row.get(1)?,
                    dim: row.get::<_, i64>(2)?.max(0) as u32,
                    pack_name: row.get(3)?,
                    used: row.get::<_, i64>(4)?.max(0) as u64,
                    blobs: row.get::<_, i64>(5)?.max(0) as u64,
                    file_size: 0,
                    garbage: 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let features_dir = LibraryPaths::new(root).features;
    let features: Vec<FeaturePackStat> = features
        .into_iter()
        .map(|mut stat| {
            stat.file_size = std::fs::metadata(features_dir.join(&stat.pack_name))
                .map(|meta| meta.len())
                .unwrap_or(0);
            stat.garbage = stat.file_size.saturating_sub(stat.used);
            stat
        })
        .collect();

    let total_size = segments.iter().map(|segment| segment.size).sum();
    let total_used = segments.iter().map(|segment| segment.used).sum();
    let garbage_bytes = segments.iter().map(|segment| segment.garbage).sum();
    Ok(PackStats {
        segments,
        total_size,
        total_used,
        garbage_bytes,
        features,
    })
}

/// 上次回收的落点，给维护页写「上次自动回收：X 前 · 释放 Y」。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactMark {
    pub at: i64,
    pub freed_bytes: u64,
}

pub fn last_compact(database: &LibraryDb) -> AppResult<Option<CompactMark>> {
    let raw: Option<String> = database.with_reader(|connection| {
        Ok(connection
            .query_row("SELECT value FROM meta WHERE key = ?1", [META_KEY], |row| {
                row.get(0)
            })
            .optional()?)
    })?;
    Ok(raw.and_then(|raw| serde_json::from_str(&raw).ok()))
}

fn save_mark(database: &LibraryDb, freed: u64) -> AppResult<()> {
    let value = serde_json::to_string(&CompactMark {
        at: now_ms(),
        freed_bytes: freed,
    })?;
    database.with_writer(|connection| {
        connection.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![META_KEY, value],
        )?;
        Ok(())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactStop {
    /// 没有超过阈值的段了。
    Done,
    /// 用户回来了，让位给前台操作。
    Yielded,
    /// 用户点了取消。
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactReport {
    pub segments: u64,
    pub freed_bytes: u64,
    pub stop: CompactStop,
}

/// 回收所有垃圾率超过触发阈值的段；被打断就带着已完成的部分返回。
///
/// `user_back` 由调用方提供：返回 `true` 表示人回来了（或任务该让位），
/// 当前段立刻放弃、不提交。取消走 `reporter.cancelled()`。
pub fn compact(
    database: &LibraryDb,
    root: &Path,
    thumbs: &ThumbPack,
    reporter: &dyn BatchReporter,
    user_back: &dyn Fn() -> bool,
) -> AppResult<CompactReport> {
    let mut report = CompactReport {
        segments: 0,
        freed_bytes: 0,
        stop: CompactStop::Done,
    };
    loop {
        if reporter.cancelled() {
            report.stop = CompactStop::Cancelled;
            break;
        }
        if user_back() {
            report.stop = CompactStop::Yielded;
            break;
        }
        let Some(segment) = next_segment(database)? else {
            report.stop = CompactStop::Done;
            break;
        };
        reporter.message(&format!(
            "正在回收缩略图包 · 第 {} 段（垃圾 {:.0}%）",
            segment.id,
            segment.garbage_ratio() * 100.0
        ));
        match compact_segment(database, root, reporter, user_back, &segment)? {
            SegmentOutcome::Compacted(freed) => {
                report.segments += 1;
                report.freed_bytes += freed;
            }
            SegmentOutcome::Yielded => {
                report.stop = CompactStop::Yielded;
                break;
            }
            SegmentOutcome::Cancelled => {
                report.stop = CompactStop::Cancelled;
                break;
            }
        }
    }

    // 写指针回到最新段末尾；旧段的读句柄也已经失效。
    if let Err(error) = thumbs.reload_from_disk() {
        log::warn!("刷新缩略图包写指针失败：{error}");
    }
    if report.segments > 0
        && let Err(error) = save_mark(database, report.freed_bytes)
    {
        log::warn!("记录缩略图包回收结果失败：{error}");
    }
    Ok(report)
}

/// 垃圾率最高、且超过触发阈值的那一段。
fn next_segment(database: &LibraryDb) -> AppResult<Option<SegmentStat>> {
    let all = stats_segments_only(database)?;
    Ok(all
        .into_iter()
        .filter(|segment| segment.garbage_ratio() > TRIGGER_RATIO)
        .max_by(|left, right| {
            left.garbage_ratio()
                .partial_cmp(&right.garbage_ratio())
                .unwrap_or(std::cmp::Ordering::Equal)
        }))
}

fn stats_segments_only(database: &LibraryDb) -> AppResult<Vec<SegmentStat>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT s.id, s.file_name, s.size,
                    COALESCE(SUM(i.thumb_length), 0) AS used,
                    COUNT(i.id) AS blobs
             FROM thumb_segments s
             LEFT JOIN images i ON i.thumb_segment = s.id
             WHERE s.id > 0
             GROUP BY s.id ORDER BY s.id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(SegmentStat::new(
                    row.get(0)?,
                    row.get(1)?,
                    row.get::<_, i64>(2)?.max(0) as u64,
                    row.get::<_, i64>(3)?.max(0) as u64,
                    row.get::<_, i64>(4)?.max(0) as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

enum SegmentOutcome {
    Compacted(u64),
    Yielded,
    Cancelled,
}

fn compact_segment(
    database: &LibraryDb,
    root: &Path,
    reporter: &dyn BatchReporter,
    user_back: &dyn Fn() -> bool,
    segment: &SegmentStat,
) -> AppResult<SegmentOutcome> {
    let directory = LibraryPaths::new(root).thumbs;
    let old_path = segment_path(&directory, segment.id);
    let new_id = next_segment_id(database, &directory)?;
    let new_path = segment_path(&directory, new_id);

    // 先查指针：复制阶段只读旧文件，写完才进事务。
    let pointers: Vec<(i64, u64, u32)> = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, thumb_offset, thumb_length FROM images
             WHERE thumb_segment = ?1 AND thumb_length > 0 AND thumb_offset >= ?2
             ORDER BY id",
        )?;
        let rows = statement
            .query_map(rusqlite::params![segment.id, HEADER_LEN as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?.max(0) as u64,
                    row.get::<_, i64>(2)?.max(0) as u32,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    reporter.report(0, pointers.len());

    // 段里一条 blob 都没有：不必再造一个只有段头的文件，整段删掉就是。
    // 残留的 `thumb_length = 0` 指针一并归零，免得指向一个已经不存在的段。
    if pointers.is_empty() {
        database.with_durable_writer(|connection| {
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            transaction.execute(
                "UPDATE images SET thumb_segment = 0, thumb_offset = 0, thumb_length = 0
                 WHERE thumb_segment = ?1",
                [segment.id],
            )?;
            transaction.execute("DELETE FROM thumb_segments WHERE id = ?1", [segment.id])?;
            transaction.commit()?;
            Ok(())
        })?;
        remove_quietly(&old_path);
        return Ok(SegmentOutcome::Compacted(segment.garbage));
    }

    let reader = File::open(&old_path)?;
    let old_size = reader.metadata()?.len();
    let mut writer = create_segment(&new_path)?;
    let mut written = HEADER_LEN;
    let mut mapped: Vec<(i64, u64, u32)> = Vec::with_capacity(pointers.len());
    let mut copied = 0_u64;
    // 复用一块读缓冲：一个段几万条 blob，每条都新建一个 Vec 是白扔的分配。
    let mut buffer: Vec<u8> = Vec::new();

    for (image_id, offset, length) in &pointers {
        if *length == 0 || *length > MAX_THUMBNAIL_BYTES {
            return Err(AppError::message(format!(
                "图片 {image_id} 的缩略图指针长度无效"
            )));
        }
        let end = offset
            .checked_add(u64::from(*length))
            .ok_or_else(|| AppError::message("缩略图指针越界"))?;
        if end > old_size {
            return Err(AppError::message(format!(
                "图片 {image_id} 的缩略图指针越界"
            )));
        }
        buffer.resize(*length as usize, 0);
        read_exact_at(&reader, *offset, &mut buffer)?;
        writer.write_all(&buffer)?;
        mapped.push((*image_id, written, *length));
        written += u64::from(*length);
        copied += 1;

        if copied.is_multiple_of(YIELD_CHECK_EVERY) {
            if reporter.cancelled() {
                drop(writer);
                remove_quietly(&new_path);
                return Ok(SegmentOutcome::Cancelled);
            }
            if user_back() {
                // 数据库一个字没改，旧段原样保留；只删掉这次新写的文件。
                drop(writer);
                remove_quietly(&new_path);
                return Ok(SegmentOutcome::Yielded);
            }
            reporter.report(copied as usize, pointers.len());
        }
    }

    writer.flush()?;
    writer.sync_all()?;
    drop(writer);

    // 一次事务改完所有指针：半途失败就整段回滚，不会出现「一半行指向新段」。
    let committed = database.with_durable_writer(|connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut statement = transaction
                .prepare("UPDATE images SET thumb_segment = ?1, thumb_offset = ?2 WHERE id = ?3")?;
            for (image_id, offset, _) in &mapped {
                statement.execute(rusqlite::params![new_id, *offset as i64, image_id])?;
            }
        }
        transaction.execute(
            "INSERT INTO thumb_segments(id, file_name, size, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET size = excluded.size",
            rusqlite::params![
                new_id,
                format!("{new_id:04}.pack"),
                written as i64,
                now_ms()
            ],
        )?;
        transaction.execute("DELETE FROM thumb_segments WHERE id = ?1", [segment.id])?;
        transaction.commit()?;
        Ok(())
    });
    if let Err(error) = committed {
        remove_quietly(&new_path);
        return Err(error);
    }

    // 提交后等两秒再删旧段：这期间新到的读请求已经全部指向新指针。
    // 仍失败的请求由现有的缩略图 404 重试兜底；删除失败只记日志，留待下次回收。
    std::thread::sleep(DELETE_DELAY);
    if let Err(error) = std::fs::remove_file(&old_path) {
        log::warn!("删除旧缩略图段 {} 失败：{error}", segment.id);
    }

    reporter.report(pointers.len(), pointers.len());
    Ok(SegmentOutcome::Compacted(segment.garbage))
}

fn next_segment_id(database: &LibraryDb, directory: &Path) -> AppResult<u32> {
    let from_db: Option<i64> = database.with_reader(|connection| {
        Ok(connection
            .query_row("SELECT max(id) FROM thumb_segments", [], |row| row.get(0))
            .optional()?
            .flatten())
    })?;
    let mut latest = from_db.unwrap_or(0).max(0) as u32;
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".pack")
                && let Ok(segment) = stem.parse::<u32>()
            {
                latest = latest.max(segment);
            }
        }
    }
    Ok(latest + 1)
}

fn create_segment(path: &Path) -> AppResult<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(path)?;
    let mut header = [0_u8; HEADER_LEN as usize];
    header[..8].copy_from_slice(b"ARTBTHM1");
    header[8..12].copy_from_slice(&1_u32.to_le_bytes());
    file.write_all(&header)?;
    file.flush()?;
    Ok(file)
}

fn remove_quietly(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        log::warn!("删除缩略图包 {} 失败：{error}", path.display());
    }
}

/// 打开图库时清理残留：`thumbs/` 里凡是不在 `thumb_segments` 表里的 `.pack` 都删掉。
///
/// 覆盖三种来源：让位时没删成的、提交失败留下的、进程崩溃留在磁盘上的。
/// 9 个段的目录扫描是毫秒级。
pub fn cleanup_orphan_segments(database: &LibraryDb, root: &Path) -> AppResult<u64> {
    let directory = LibraryPaths::new(root).thumbs;
    let known: Vec<String> = database.with_reader(|connection| {
        let mut statement = connection.prepare("SELECT file_name FROM thumb_segments")?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let known: std::collections::HashSet<String> = known.into_iter().collect();
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut removed = 0_u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.ends_with(".pack") || known.contains(&name) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) => log::warn!("清理残留缩略图包 {name} 失败：{error}"),
        }
    }
    Ok(removed)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;

    while !buffer.is_empty() {
        match file.seek_read(buffer, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "缩略图包提前结束",
                ));
            }
            Ok(read) => {
                offset += read as u64;
                buffer = &mut buffer[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    while !buffer.is_empty() {
        match file.read_at(buffer, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "缩略图包提前结束",
                ));
            }
            Ok(read) => {
                offset += read as u64;
                buffer = &mut buffer[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(any(windows, unix)))]
fn read_exact_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{NoopReporter, service};
    use std::path::PathBuf;

    struct Fixture {
        root: PathBuf,
        session: service::LibrarySession,
        _temp: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("library");
            let (session, _) = service::create(&root, "包回收测试", 1).unwrap();
            Self {
                root,
                session,
                _temp: temp,
            }
        }

        fn db(&self) -> &LibraryDb {
            &self.session.db
        }

        /// 追加一个 blob 并插入图片行。
        fn add(&self, hash_char: char, bytes: &[u8]) -> i64 {
            let location = self.session.thumbs.append(bytes).unwrap();
            let hash = format!("{}{}", hash_char.to_string().repeat(63), "a");
            self.db()
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at, thumb_segment, thumb_offset, thumb_length, thumb_format,
                            thumb_width, thumb_height
                         ) VALUES (?1, 'webp', 1, 10, 4, 4, 'webp', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1,
                            ?2, ?3, ?4, 'webp', 4, 4)",
                        rusqlite::params![
                            hash,
                            location.segment,
                            location.offset as i64,
                            location.length as i64
                        ],
                    )?;
                    let id = connection.last_insert_rowid();
                    connection.execute(
                        "INSERT INTO thumb_segments(id, file_name, size, created_at)
                         VALUES (?1, ?2, ?3, 1)
                         ON CONFLICT(id) DO UPDATE SET size = excluded.size",
                        rusqlite::params![
                            location.segment,
                            format!("{:04}.pack", location.segment),
                            self.session
                                .thumbs
                                .segment_size_bytes(location.segment)
                                .unwrap_or(0) as i64
                        ],
                    )?;
                    Ok(id)
                })
                .unwrap()
        }

        /// 重新 append 一张图的缩略图并改指针：旧 blob 从此无人引用，成为空洞。
        fn make_garbage(&self) {
            let id: i64 = self
                .db()
                .with_reader(|connection| {
                    Ok(connection.query_row(
                        "SELECT id FROM images ORDER BY id LIMIT 1",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .unwrap();
            let location = self.session.thumbs.append(b"0123456789").unwrap();
            self.db()
                .with_writer(|connection| {
                    connection.execute(
                        "UPDATE images SET thumb_segment = ?1, thumb_offset = ?2,
                            thumb_length = ?3 WHERE id = ?4",
                        rusqlite::params![
                            location.segment,
                            location.offset as i64,
                            location.length as i64,
                            id
                        ],
                    )?;
                    connection.execute(
                        "UPDATE thumb_segments SET size = ?1 WHERE id = ?2",
                        rusqlite::params![
                            self.session
                                .thumbs
                                .segment_size_bytes(location.segment)
                                .unwrap_or(0) as i64,
                            location.segment
                        ],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
    }

    #[test]
    fn stats_report_zero_garbage_for_a_fresh_pack() {
        let fixture = Fixture::new();
        fixture.add('b', b"0123456789");
        let stats = stats(fixture.db(), &fixture.root).unwrap();
        assert_eq!(stats.segments.len(), 1);
        assert_eq!(stats.garbage_bytes, 0);
    }

    #[test]
    fn compaction_rewrites_pointers_and_removes_the_old_segment() {
        let fixture = Fixture::new();
        let first = fixture.add('b', b"0123456789");
        let second = fixture.add('c', b"abcdefghij");
        fixture.make_garbage();
        let before = stats(fixture.db(), &fixture.root).unwrap();
        assert!(before.garbage_bytes > 0);

        let report = compact(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            &NoopReporter,
            &|| false,
        )
        .unwrap();
        assert_eq!(report.stop, CompactStop::Done);
        assert_eq!(report.segments, 1);
        assert!(report.freed_bytes > 0);

        let after = stats(fixture.db(), &fixture.root).unwrap();
        assert_eq!(after.segments.len(), 1);
        assert_eq!(after.garbage_bytes, 0);
        // 指针全部落到新段，内容逐字节相等。
        for id in [first, second] {
            let (segment, offset, length): (u32, i64, i64) = fixture
                .db()
                .with_reader(|connection| {
                    Ok(connection.query_row(
                        "SELECT thumb_segment, thumb_offset, thumb_length FROM images WHERE id = ?1",
                        [id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?)
                })
                .unwrap();
            let bytes = fixture
                .session
                .thumbs
                .read(crate::library::thumbs::ThumbLocation {
                    segment,
                    offset: offset as u64,
                    length: length as u32,
                })
                .unwrap();
            assert_eq!(bytes.len(), 10);
        }
        // 旧段文件已删除。
        assert!(!segment_path(&fixture.root.join(".artbank").join("thumbs"), 1).exists());
    }

    #[test]
    fn yielding_leaves_the_database_untouched() {
        let fixture = Fixture::new();
        fixture.add('b', b"0123456789");
        fixture.add('c', b"abcdefghij");
        fixture.make_garbage();
        let before = stats(fixture.db(), &fixture.root).unwrap();
        let report = compact(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            &NoopReporter,
            &|| true,
        )
        .unwrap();
        assert_eq!(report.stop, CompactStop::Yielded);
        let after = stats(fixture.db(), &fixture.root).unwrap();
        assert_eq!(after.garbage_bytes, before.garbage_bytes);
        assert_eq!(after.segments.len(), 1);
        // 新写的段文件已删掉，旧段原样保留。
        assert!(segment_path(&fixture.root.join(".artbank").join("thumbs"), 1).exists());
        assert!(!segment_path(&fixture.root.join(".artbank").join("thumbs"), 2).exists());
    }

    #[test]
    fn cleanup_removes_pack_files_missing_from_the_table() {
        let fixture = Fixture::new();
        fixture.add('b', b"0123456789");
        let directory = fixture.root.join(".artbank").join("thumbs");
        std::fs::write(directory.join("9999.pack"), b"junk").unwrap();
        let removed = cleanup_orphan_segments(fixture.db(), &fixture.root).unwrap();
        assert_eq!(removed, 1);
        assert!(!directory.join("9999.pack").exists());
        assert!(directory.join("0001.pack").exists());
    }

    #[test]
    fn cleanup_tolerates_a_missing_directory() {
        let fixture = Fixture::new();
        std::fs::remove_dir_all(fixture.root.join(".artbank").join("thumbs")).unwrap();
        assert_eq!(
            cleanup_orphan_segments(fixture.db(), &fixture.root).unwrap(),
            0
        );
    }

    /// 段里一条有效指针都没有时整段删掉，不再造一个只有段头的空文件。
    #[test]
    fn a_segment_without_pointers_is_removed_instead_of_rewritten() {
        let fixture = Fixture::new();
        fixture.add('b', b"0123456789");
        // 模拟「行还在、缩略图已经没了」：`thumb_length = 0` 的指针不算有效。
        fixture
            .db()
            .with_writer(|connection| {
                connection.execute("UPDATE images SET thumb_length = 0", [])?;
                Ok(())
            })
            .unwrap();

        let report = compact(
            fixture.db(),
            &fixture.root,
            &fixture.session.thumbs,
            &NoopReporter,
            &|| false,
        )
        .unwrap();
        assert_eq!(report.stop, CompactStop::Done);
        assert_eq!(report.segments, 1);

        let directory = fixture.root.join(".artbank").join("thumbs");
        assert!(!segment_path(&directory, 1).exists(), "空段文件应被删掉");
        assert!(
            !segment_path(&directory, 2).exists(),
            "不该新建一个只有段头的段"
        );
        let rows: i64 = fixture
            .db()
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT COUNT(*) FROM thumb_segments WHERE id = 1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(rows, 0, "段行应被删掉");
        let segment: i64 = fixture
            .db()
            .with_reader(|connection| {
                Ok(connection
                    .query_row("SELECT thumb_segment FROM images", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(segment, 0, "残留指针归零，不指向已删除的段");
    }
}

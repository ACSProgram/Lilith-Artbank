//! 批量编辑：一套 recipe 应用到选中集，逐张准备/提交，整批一条操作记录。

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::{
    app::error::{AppError, AppResult},
    concurrency::{
        budget::ImageBudget,
        resources::{HolderInfo, ResourceTable, WaitError},
    },
    imageops::{
        edit as image_edit,
        edit::{CanvasFit, CropRect, FitMode, Geometry, MAX_EDIT_PIXELS, OutputFormat},
    },
    library::{
        BatchReporter, content,
        db::LibraryDb,
        edit as library_edit,
        edit::StageOptions,
        history::{self, BatchEditRecord, ChunkFormat, HistoryKind, LiveOperation, NewOperation},
        paths::LibraryPaths,
        temp,
        thumbs::ThumbPack,
    },
    platform,
};

/// 超过这个张数就不再保存逐张撤销副本；前端确认后传 `reversible: false`。
pub const MAX_REVERSIBLE_IMAGES: usize = 2000;

const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BatchEditMode {
    /// 每张保持翻转/旋转后的原始尺寸，不做画布适配与裁剪。
    KeepSize,
    /// 所有图片先适配同一块画布，再按画布坐标裁剪。
    Canvas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BatchFit {
    Cover,
    Contain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BatchOutputFormat {
    Keep,
    Jpeg,
    Png,
    Webp,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchCanvas {
    pub width: u32,
    pub height: u32,
    pub fit: BatchFit,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEditOptions {
    #[serde(default)]
    pub rotate: u8,
    #[serde(default)]
    pub flip_horizontal: bool,
    #[serde(default)]
    pub flip_vertical: bool,
    pub mode: BatchEditMode,
    pub canvas: BatchCanvas,
    #[serde(default)]
    pub crop: Option<CropRect>,
    pub output_format: BatchOutputFormat,
    #[serde(default)]
    pub quality: Option<u8>,
    #[serde(default)]
    pub reversible: bool,
}

impl BatchEditOptions {
    pub fn validate(&self) -> AppResult<()> {
        Geometry {
            rotate: self.rotate,
            flip_horizontal: self.flip_horizontal,
            flip_vertical: self.flip_vertical,
            crop: self.crop,
            resize: None,
        }
        .validate()?;
        if let Some(quality) = self.quality
            && !(1..=100).contains(&quality)
        {
            return Err(AppError::message("输出质量必须在 1-100 之间"));
        }
        match self.mode {
            BatchEditMode::KeepSize => {
                if self.crop.is_some() {
                    return Err(AppError::message("保持各自尺寸时不能裁剪"));
                }
            }
            BatchEditMode::Canvas => {
                CanvasFit {
                    width: self.canvas.width,
                    height: self.canvas.height,
                    mode: match self.canvas.fit {
                        BatchFit::Cover => FitMode::Cover,
                        BatchFit::Contain => FitMode::Contain,
                    },
                }
                .validate()?;
                if let Some(crop) = self.crop {
                    image_edit::validate_crop_public(crop, self.canvas.width, self.canvas.height)?;
                }
            }
        }
        Ok(())
    }

    fn output_format_for(&self, row: &content::ContentRow) -> OutputFormat {
        match self.output_format {
            BatchOutputFormat::Keep => OutputFormat::default_for(&row.format, row.has_alpha),
            BatchOutputFormat::Jpeg => OutputFormat::Jpeg,
            BatchOutputFormat::Png => OutputFormat::Png,
            BatchOutputFormat::Webp => OutputFormat::WebP,
        }
    }

    fn quality_for(&self, output: OutputFormat) -> u8 {
        self.quality
            .unwrap_or_else(|| image_edit::default_quality(output))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    Missing,
    Trashed,
    Temp,
    Animated,
    Oversize,
    Unchanged,
}

impl SkipReason {
    fn key(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Trashed => "trashed",
            Self::Temp => "temp",
            Self::Animated => "animated",
            Self::Oversize => "oversize",
            Self::Unchanged => "unchanged",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEditPreview {
    pub total: u64,
    pub will_process: u64,
    pub skip_breakdown: Vec<(String, u64)>,
    pub estimated_output_bytes: u64,
    pub available_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEditFailure {
    pub image_id: i64,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchEditReport {
    pub processed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub skip_breakdown: Vec<(String, u64)>,
    pub failures: Vec<BatchEditFailure>,
    pub cancelled: bool,
}

#[derive(Default)]
struct Skips(BTreeMap<&'static str, u64>);

impl Skips {
    fn add(&mut self, reason: SkipReason) {
        *self.0.entry(reason.key()).or_insert(0) += 1;
    }

    fn buckets(&self) -> Vec<(String, u64)> {
        self.0
            .iter()
            .map(|(key, count)| ((*key).to_owned(), *count))
            .collect()
    }

    fn total(&self) -> u64 {
        self.0.values().copied().sum()
    }
}

struct Plan {
    geometry: Geometry,
    fit: Option<CanvasFit>,
    output_format: OutputFormat,
    quality: u8,
    output_width: u32,
    output_height: u32,
    pad: [u8; 4],
    changed: bool,
}

fn transformed_size(width: u32, height: u32, rotate: u8) -> (u32, u32) {
    if rotate.is_multiple_of(2) {
        (width, height)
    } else {
        (height, width)
    }
}

fn plan(row: &content::ContentRow, options: &BatchEditOptions) -> AppResult<Plan> {
    let (transformed_width, transformed_height) =
        transformed_size(row.width, row.height, options.rotate);
    let output_format = options.output_format_for(row);
    let quality = options.quality_for(output_format);
    let pad = if output_format != OutputFormat::Jpeg && row.has_alpha {
        [0, 0, 0, 0]
    } else {
        [255, 255, 255, 255]
    };

    let (fit, crop, output_width, output_height, fit_changes) = match options.mode {
        BatchEditMode::Canvas => {
            let fit = CanvasFit {
                width: options.canvas.width,
                height: options.canvas.height,
                mode: match options.canvas.fit {
                    BatchFit::Cover => FitMode::Cover,
                    BatchFit::Contain => FitMode::Contain,
                },
            };
            fit.validate()?;
            if let Some(crop) = options.crop {
                image_edit::validate_crop_public(crop, fit.width, fit.height)?;
            }
            let (width, height) = options
                .crop
                .map(|crop| (crop.width, crop.height))
                .unwrap_or((fit.width, fit.height));
            let aspect_changes = u64::from(fit.width) * u64::from(transformed_height)
                != u64::from(fit.height) * u64::from(transformed_width);
            let fit_changes = aspect_changes
                || (fit.width, fit.height) != (transformed_width, transformed_height)
                || options.crop.is_some();
            (Some(fit), options.crop, width, height, fit_changes)
        }
        BatchEditMode::KeepSize => (None, None, transformed_width, transformed_height, false),
    };

    image_edit::validate_resize_public(output_width, output_height)?;

    let geometry = Geometry {
        rotate: options.rotate,
        flip_horizontal: options.flip_horizontal,
        flip_vertical: options.flip_vertical,
        crop,
        resize: None,
    };
    // 与单图保存共用“是否有改动”的口径；画布适配是批量独有的一步，单独并入。
    let changed = image_edit::has_changes_with_default(
        &geometry,
        &row.format,
        output_format,
        quality,
        image_edit::default_quality(output_format),
    ) || fit_changes;

    Ok(Plan {
        geometry,
        fit,
        output_format,
        quality,
        output_width,
        output_height,
        pad,
        changed,
    })
}

fn preview_skip(row: &content::ContentRow, temp_ids: &HashSet<i64>) -> Option<SkipReason> {
    if row.deleted_at.is_some() {
        return Some(SkipReason::Trashed);
    }
    if temp_ids.contains(&row.id) {
        return Some(SkipReason::Temp);
    }
    if row.format.eq_ignore_ascii_case("gif") {
        return Some(SkipReason::Animated);
    }
    if row.pixel_count() > MAX_EDIT_PIXELS {
        return Some(SkipReason::Oversize);
    }
    None
}

fn load_temp_ids(database: &LibraryDb, ids: &[i64]) -> AppResult<HashSet<i64>> {
    database.with_reader(|connection| temp::image_ids_in(connection, ids))
}

/// 预检：逐张读行，统计可处理数、跳过原因与体积。
///
/// 只累积计数，不把整批行留在内存里。
pub fn preview(
    database: &LibraryDb,
    root: &Path,
    ids: &[i64],
    options: &BatchEditOptions,
) -> AppResult<BatchEditPreview> {
    options.validate()?;
    let temp_ids = load_temp_ids(database, ids)?;
    let mut skips = Skips::default();
    let mut will_process = 0_u64;
    let mut estimated_output_bytes = 0_u64;

    for id in ids {
        let row = match content::load(database, *id) {
            Ok(row) => row,
            Err(_) => {
                skips.add(SkipReason::Missing);
                continue;
            }
        };
        if let Some(reason) = preview_skip(&row, &temp_ids) {
            skips.add(reason);
            continue;
        }

        let plan = plan(&row, options)?;
        if !plan.changed {
            skips.add(SkipReason::Unchanged);
            continue;
        }
        will_process += 1;
        estimated_output_bytes = estimated_output_bytes.saturating_add(estimated_bytes(
            &row,
            plan.output_format,
            plan.output_width,
            plan.output_height,
        ));
    }

    Ok(BatchEditPreview {
        total: ids.len() as u64,
        will_process,
        skip_breakdown: skips.buckets(),
        estimated_output_bytes,
        available_bytes: platform::fs::available_bytes(root),
    })
}

fn estimated_bytes(
    row: &content::ContentRow,
    output: OutputFormat,
    width: u32,
    height: u32,
) -> u64 {
    let output_pixels = u64::from(width) * u64::from(height);
    let source_pixels = row.pixel_count().max(1);
    let source_bpp = row.file_size.max(0) as f64 / source_pixels as f64;
    let format_bpp = match output {
        OutputFormat::Jpeg => 0.55,
        OutputFormat::WebP => 0.35,
        OutputFormat::Png => 2.0,
    };
    let bpp = match row.format.as_str() {
        "jpeg" | "jpg" if output == OutputFormat::Jpeg => source_bpp,
        "png" if output == OutputFormat::Png => source_bpp,
        "webp" if output == OutputFormat::WebP => source_bpp,
        _ => format_bpp,
    };
    ((output_pixels as f64 * bpp).round() as u64).max(1)
}

/// 执行批量编辑；每张一个事务，整批共用 `operation_id`。
#[allow(clippy::too_many_arguments)]
pub fn run(
    database: &Arc<LibraryDb>,
    root: &Path,
    thumbs: &ThumbPack,
    ids: &[i64],
    options: &BatchEditOptions,
    operation_id: i64,
    reporter: &dyn BatchReporter,
    resources: &ResourceTable,
    images: &ImageBudget,
) -> AppResult<BatchEditReport> {
    options.validate()?;
    if options.reversible && ids.len() > MAX_REVERSIBLE_IMAGES {
        return Err(AppError::message(format!(
            "超过 {MAX_REVERSIBLE_IMAGES} 张的批量编辑不能保留撤销副本"
        )));
    }
    if options.reversible {
        std::fs::create_dir_all(history::undo_dir(root, operation_id))?;
    }

    let paths = LibraryPaths::new(root);
    let temp_ids = load_temp_ids(database, ids)?;
    let total = ids.len();
    let mut report = BatchEditReport::default();
    let mut skips = Skips::default();
    let mut first_write = true;
    let holder = HolderInfo::new(format!("batch-edit-{operation_id}"), "批量编辑")
        .with_path_note("正在写入");

    for id in ids {
        if reporter.cancelled() {
            report.cancelled = true;
            break;
        }
        reporter.current(Some(*id), None);

        let mut row = match content::load(database, *id) {
            Ok(row) => row,
            Err(_) => {
                skips.add(SkipReason::Missing);
                reporter.report(report.processed as usize, total);
                reporter.skipped_count(skips.total() as usize);
                reporter.skip_breakdown(&skips.buckets());
                continue;
            }
        };
        if let Some(reason) = preview_skip(&row, &temp_ids) {
            skips.add(reason);
            reporter.report(report.processed as usize, total);
            reporter.skipped_count(skips.total() as usize);
            reporter.skip_breakdown(&skips.buckets());
            continue;
        }
        let plan = match plan(&row, options) {
            Ok(plan) => plan,
            Err(error) => {
                record_failure(&mut report, &row, error.to_string());
                reporter.report(report.processed as usize, total);
                reporter.failed_count(report.failed as usize);
                continue;
            }
        };
        if !plan.changed {
            skips.add(SkipReason::Unchanged);
            reporter.report(report.processed as usize, total);
            reporter.skipped_count(skips.total() as usize);
            reporter.skip_breakdown(&skips.buckets());
            continue;
        }

        // 逐张锁住图片所在文件夹；提交后立刻释放，不把整批文件夹都占住。
        let mut locked = None;
        let mut lock_error: Option<String> = None;
        for _ in 0..3 {
            let guard = match resources
                .acquire_path_write(&row.folder_path, &holder, &|| reporter.cancelled())
            {
                Ok(guard) => guard,
                Err(WaitError::Cancelled) => {
                    report.cancelled = true;
                    break;
                }
                Err(WaitError::Timeout(_)) => {
                    lock_error = Some("原图所在文件夹正忙，请稍后重试".to_owned());
                    break;
                }
            };
            let latest = match content::load(database, *id) {
                Ok(latest) => latest,
                Err(error) => {
                    lock_error = Some(error.to_string());
                    break;
                }
            };
            if latest.folder_path.eq_ignore_ascii_case(&row.folder_path)
                && latest.content_hash == row.content_hash
            {
                row = latest;
                locked = Some(guard);
                break;
            }
            drop(guard);
            row = latest;
        }
        if report.cancelled {
            break;
        }
        let Some(_guard) = locked else {
            record_failure(
                &mut report,
                &row,
                lock_error.unwrap_or_else(|| "图片位置正在变化，请稍后重试".to_owned()),
            );
            reporter.report(report.processed as usize, total);
            reporter.failed_count(report.failed as usize);
            continue;
        };

        let permit = match images.acquire(&|| reporter.cancelled()) {
            Ok(permit) => permit,
            Err(_) => {
                report.cancelled = true;
                break;
            }
        };

        let source_path = match row.path(&paths) {
            Ok(path) => path,
            Err(error) => {
                record_failure(&mut report, &row, error.to_string());
                reporter.report(report.processed as usize, total);
                reporter.failed_count(report.failed as usize);
                continue;
            }
        };
        let mut prepared = match library_edit::prepare(
            root,
            &row,
            &plan.geometry,
            plan.fit,
            plan.pad,
            plan.output_format,
            plan.quality,
        ) {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                skips.add(SkipReason::Unchanged);
                reporter.report(report.processed as usize, total);
                reporter.skipped_count(skips.total() as usize);
                reporter.skip_breakdown(&skips.buckets());
                continue;
            }
            Err(error) => {
                record_failure(&mut report, &row, error.to_string());
                reporter.report(report.processed as usize, total);
                reporter.failed_count(report.failed as usize);
                continue;
            }
        };

        let stage = options.reversible.then_some(StageOptions {
            operation_id,
            create_dir: false,
        });
        let commit = history::write_live(database, first_write, |transaction, journal| {
            let snapshot = library_edit::commit_prepared(
                transaction,
                root,
                thumbs,
                &mut prepared,
                journal,
                stage,
            )?;
            if first_write {
                history::append_live_in_tx(
                    transaction,
                    operation_id,
                    ChunkFormat::Json,
                    &serde_json::to_vec(&BatchEditRecord::Meta {
                        reversible: options.reversible,
                    })?,
                )?;
            }
            history::append_live_in_tx(
                transaction,
                operation_id,
                ChunkFormat::Json,
                &serde_json::to_vec(&BatchEditRecord::Snapshot {
                    snapshot: Box::new(snapshot),
                })?,
            )?;
            history::add_live_affected_in_tx(transaction, operation_id, 1)?;
            Ok(())
        });
        match commit {
            Ok(()) => {
                first_write = false;
                report.processed += 1;
                reporter.report(report.processed as usize, total);
                reporter.message(&format!("已编辑 {}", report.processed));
                // 可撤销批次里旧原图已在撤销目录留了硬链接，这里只清理原路径。
                library_edit::remove_old_original(database, root, &row, &source_path);
                drop(permit);
            }
            Err(error) => {
                record_failure(&mut report, &row, error.to_string());
                reporter.report(report.processed as usize, total);
                reporter.failed_count(report.failed as usize);
            }
        }
    }

    report.skipped = skips.total();
    report.skip_breakdown = skips.buckets();
    reporter.report(report.processed as usize, total);
    reporter.skipped_count(report.skipped as usize);
    reporter.skip_breakdown(&report.skip_breakdown);
    reporter.failed_count(report.failed as usize);
    Ok(report)
}

fn record_failure(report: &mut BatchEditReport, row: &content::ContentRow, reason: String) {
    report.failed += 1;
    report.failures.push(BatchEditFailure {
        image_id: row.id,
        path: row.relative_display_path(),
        reason,
    });
}

/// 开始前的资源预检：体积超过可用空间时直接拒绝，任务不进队列。
pub fn ensure_disk_space(preview: &BatchEditPreview) -> AppResult<()> {
    if let Some(available) = preview.available_bytes
        && preview.estimated_output_bytes > available
    {
        return Err(AppError::message(format!(
            "预计需要约 {} MB，但磁盘只剩约 {} MB；请缩小选择或先释放空间",
            preview.estimated_output_bytes / MIB,
            available / MIB
        )));
    }
    Ok(())
}

/// 应用层创建 `LiveOperation` 时用同一套标题。
pub fn operation(database: &Arc<LibraryDb>, count: usize) -> AppResult<LiveOperation> {
    LiveOperation::begin(
        database,
        NewOperation::new(HistoryKind::BatchEdit, format!("批量编辑 · {count} 张")),
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::{
        app::tasks::TaskKind,
        concurrency::{budget::ImageBudget, resources::ResourceTable},
        library::{NoopReporter, history, metadata, service},
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        session: service::LibrarySession,
        root: PathBuf,
        ids: Vec<i64>,
        old_hashes: Vec<String>,
        old_paths: Vec<PathBuf>,
    }

    fn build_library(sizes: &[(u32, u32)]) -> Fixture {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "批量编辑测试", 1).unwrap();
        let mut ids = Vec::new();
        let mut old_hashes = Vec::new();
        let mut old_paths = Vec::new();
        for (index, (width, height)) in sizes.iter().enumerate() {
            let mut image = image::RgbImage::new(*width, *height);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                *pixel = image::Rgb([
                    ((x * 7 + index as u32) % 256) as u8,
                    ((y * 11 + index as u32) % 256) as u8,
                    ((x + y + index as u32) % 256) as u8,
                ]);
            }
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgb8(image)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .unwrap();
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let path = metadata::content_path(&LibraryPaths::new(&root), "", &hash, "png").unwrap();
            fs::write(&path, &bytes).unwrap();
            let id = session
                .db
                .with_writer(|connection| {
                    connection.execute(
                        "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at, thumb_version
                         ) VALUES (?1, 'png', 1, ?2, ?3, ?4, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1, 1)",
                        params![hash, bytes.len() as i64, *width, *height],
                    )?;
                    Ok(connection.last_insert_rowid())
                })
                .unwrap();
            ids.push(id);
            old_hashes.push(hash);
            old_paths.push(path);
        }
        Fixture {
            _temp: temp,
            session,
            root,
            ids,
            old_hashes,
            old_paths,
        }
    }

    fn options(reversible: bool) -> BatchEditOptions {
        BatchEditOptions {
            rotate: 1,
            flip_horizontal: false,
            flip_vertical: false,
            mode: BatchEditMode::Canvas,
            canvas: BatchCanvas {
                width: 12,
                height: 12,
                fit: BatchFit::Cover,
            },
            crop: None,
            output_format: BatchOutputFormat::Png,
            quality: None,
            reversible,
        }
    }

    fn row_hash(fixture: &Fixture, id: i64) -> String {
        fixture
            .session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT content_hash FROM images WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )?)
            })
            .unwrap()
    }

    fn row_size(fixture: &Fixture, id: i64) -> (u32, u32) {
        fixture
            .session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT width, height FROM images WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .unwrap()
    }

    #[test]
    fn batch_edit_is_one_record_and_undo_redo_restores_content() {
        let fixture = build_library(&[(8, 6), (10, 4)]);
        let operation = operation(&fixture.session.db, fixture.ids.len()).unwrap();
        let operation_id = operation.id();
        let report = run(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            &fixture.ids,
            &options(true),
            operation_id,
            &NoopReporter,
            &ResourceTable::new(),
            &ImageBudget::new(2),
        )
        .unwrap();
        operation.finish("").unwrap();
        assert_eq!(report.processed, 2);
        assert_eq!(report.failed, 0);
        assert_eq!(report.skipped, 0);
        for id in &fixture.ids {
            assert_eq!(row_size(&fixture, *id), (12, 12));
        }

        let entries = history::list(&fixture.session.db).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, HistoryKind::BatchEdit);
        assert_eq!(entries[0].affected, 2);

        history::run_undo(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            operation_id,
            crate::app::tasks::TaskHandle::new("undo", TaskKind::Undo, "测试"),
        )
        .unwrap();
        for (id, old) in fixture.ids.iter().zip(&fixture.old_hashes) {
            assert_eq!(row_hash(&fixture, *id), *old);
        }

        history::run_redo(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            operation_id,
            crate::app::tasks::TaskHandle::new("redo", TaskKind::Redo, "测试"),
        )
        .unwrap();
        for id in &fixture.ids {
            assert_eq!(row_size(&fixture, *id), (12, 12));
        }
    }

    #[test]
    fn non_reversible_batch_deletes_old_originals_and_refuses_undo() {
        let fixture = build_library(&[(8, 6)]);
        let operation = operation(&fixture.session.db, fixture.ids.len()).unwrap();
        let operation_id = operation.id();
        run(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            &fixture.ids,
            &options(false),
            operation_id,
            &NoopReporter,
            &ResourceTable::new(),
            &ImageBudget::new(1),
        )
        .unwrap();
        operation.finish("").unwrap();
        assert!(!fixture.old_paths[0].exists());

        let error = history::run_undo(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            operation_id,
            crate::app::tasks::TaskHandle::new("undo", TaskKind::Undo, "测试"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("没有保留撤销副本"), "{error}");
    }

    fn keep_size_options() -> BatchEditOptions {
        BatchEditOptions {
            rotate: 0,
            flip_horizontal: false,
            flip_vertical: false,
            mode: BatchEditMode::KeepSize,
            canvas: BatchCanvas {
                width: 0,
                height: 0,
                fit: BatchFit::Contain,
            },
            crop: None,
            output_format: BatchOutputFormat::Keep,
            quality: None,
            reversible: true,
        }
    }

    #[test]
    fn keep_size_preview_skips_unchanged_images() {
        let fixture = build_library(&[(8, 6), (8, 6)]);
        let preview = super::preview(
            &fixture.session.db,
            &fixture.root,
            &fixture.ids,
            &keep_size_options(),
        )
        .unwrap();
        assert_eq!(preview.will_process, 0);
        assert_eq!(
            preview
                .skip_breakdown
                .iter()
                .find(|(key, _)| key == "unchanged")
                .map(|(_, count)| *count),
            Some(2)
        );
    }

    #[test]
    fn keep_size_preserves_each_transformed_size() {
        let fixture = build_library(&[(8, 6), (10, 4)]);
        let operation = operation(&fixture.session.db, fixture.ids.len()).unwrap();
        let report = run(
            &fixture.session.db,
            &fixture.root,
            &fixture.session.thumbs,
            &fixture.ids,
            &BatchEditOptions {
                rotate: 1,
                ..keep_size_options()
            },
            operation.id(),
            &NoopReporter,
            &ResourceTable::new(),
            &ImageBudget::new(2),
        )
        .unwrap();
        operation.finish("").unwrap();
        assert_eq!(report.processed, 2);
        assert_eq!(row_size(&fixture, fixture.ids[0]), (6, 8));
        assert_eq!(row_size(&fixture, fixture.ids[1]), (4, 10));
    }

    #[test]
    fn canvas_preview_accepts_mixed_sizes_without_uniform_check() {
        let fixture = build_library(&[(8, 6), (10, 6)]);
        let preview = super::preview(
            &fixture.session.db,
            &fixture.root,
            &fixture.ids,
            &options(true),
        )
        .unwrap();
        assert_eq!(preview.will_process, 2);
    }

    #[test]
    fn keep_size_rejects_crop() {
        let error = BatchEditOptions {
            crop: Some(CropRect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }),
            ..keep_size_options()
        }
        .validate()
        .unwrap_err();
        assert!(error.to_string().contains("不能裁剪"), "{error}");
    }

    #[test]
    fn preview_skips_trashed_images() {
        let fixture = build_library(&[(8, 6)]);
        metadata::set_trashed(&fixture.session.db, &[fixture.ids[0]], true).unwrap();
        let preview = super::preview(
            &fixture.session.db,
            &fixture.root,
            &fixture.ids,
            &options(true),
        )
        .unwrap();
        assert_eq!(preview.will_process, 0);
        assert!(
            preview
                .skip_breakdown
                .iter()
                .any(|(key, count)| { key == "trashed" && *count == 1 })
        );
    }

    #[test]
    fn reversible_limit_is_enforced_before_reading_rows() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "上限测试", 1).unwrap();
        let ids: Vec<i64> = (1..=2001).collect();
        let error = run(
            &session.db,
            &root,
            &session.thumbs,
            &ids,
            &options(true),
            1,
            &NoopReporter,
            &ResourceTable::new(),
            &ImageBudget::new(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("超过 2000 张"), "{error}");
    }
}

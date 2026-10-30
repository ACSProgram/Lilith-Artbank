use std::{
    collections::HashMap,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use rayon::prelude::*;

use crate::{
    app::error::AppResult,
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget},
        resources::{HolderInfo, ResourceTable},
    },
    imageops::optimize::{self, OptimizeOptions, SourceInfo},
    library::{
        BatchReporter,
        calibration::{CONSERVATIVE as CONSERVATIVE_CALIBRATION, OptimizeCalibration},
        db::LibraryDb,
        optimize::{CandidateRow, collect_candidates},
        service::now_ms,
    },
};

use super::{
    FolderEstimate, OptimizeEstimate, QualityCount, SAMPLE_MAX,
    extrapolate::{
        BOOTSTRAP_ROUNDS, TimingInputs, WorkTotals, benefit_curve, cumulative_skips,
        estimate_seconds, margins, point_savings,
    },
    report::{PrefilterSkips, empty_estimate, failed_outcome},
    sampling::{
        Rng, SAMPLE_PERCENT_START, SampleOutcome, SampleTask, StratumEntry, allocate_samples,
        finish_progress, sample_one, sample_percentage, scan_percentage, seed, slice_indices,
    },
    strata::{STRATUM_COUNT, StratumTotals, bucket_for, stratum_index, stratum_rates},
};

const MAX_SAMPLE_FAILURES: usize = 20;
const CHUNK: usize = 400;

/// “大图”门槛：结果页拿样本与候选里 ≥ 32 MP 的张数对比，说明尾部有没有被代表。
const LARGE_IMAGE_PIXELS: u64 = 32_000_000;

#[derive(Debug, Clone, Default)]
struct FolderTotals {
    path: String,
    count: u64,
    bytes: u64,
    stratum_bytes: [u64; STRATUM_COUNT],
}
/// 估算入口：只读；返回 `None` 表示中途取消。
#[allow(clippy::too_many_arguments)]
pub fn estimate(
    database: &LibraryDb,
    root: &Path,
    task_id: &str,
    image_ids: &[i64],
    options: &OptimizeOptions,
    sample_budget: usize,
    options_fingerprint: &str,
    reporter: &dyn BatchReporter,
    resources: &ResourceTable,
    images: &ImageBudget,
    memory: &ImageMemoryBudget,
    holder: &HolderInfo,
) -> AppResult<Option<OptimizeEstimate>> {
    options.validate()?;
    let sample_budget = sample_budget.clamp(1, SAMPLE_MAX);
    let mut totals = std::array::from_fn::<_, STRATUM_COUNT, _>(|_| StratumTotals::default());
    let mut strata_entries: [Vec<StratumEntry>; STRATUM_COUNT] =
        std::array::from_fn(|_| Vec::new());
    let mut folders: HashMap<i64, FolderTotals> = HashMap::new();
    let mut total_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut candidate_large_count = 0_u64;
    let mut skips = PrefilterSkips::default();

    // 统计阶段只报百分比、不报张数：这一段的“张数”就是整个范围，当成抽样进度会让人
    // 以为在重跑全库。真正能按张数计的只有样本编码。
    let scan_total = image_ids.len();
    reporter.message("正在统计候选图片…");
    reporter.report(0, 0);
    reporter.percent(scan_percentage(0, scan_total));
    let mut scanned = 0_usize;
    for chunk in image_ids.chunks(CHUNK) {
        if reporter.cancelled() {
            return Ok(None);
        }
        let rows = collect_candidates(database, chunk)?;
        scanned += chunk.len();
        for row in rows {
            total_count += 1;
            let bytes = row.file_size.max(0) as u64;
            total_bytes += bytes;
            // 与执行阶段共用同一段前置判据；这里只有数据库列，位深按 8 位占位。
            let source = SourceInfo {
                format: row.format.clone(),
                file_size: row.file_size,
                width: row.width,
                height: row.height,
                has_alpha: row.has_alpha,
                optimized_at: row.optimized_at,
                bit_depth: 8,
            };
            if let Err(reason) = optimize::prefilter(&source, options)
                && skips.record(&reason, bytes)
            {
                continue;
            }
            let stratum = stratum_index(row.lossless_source(), bucket_for(source.bpp()));
            totals[stratum].count += 1;
            totals[stratum].bytes += bytes;
            totals[stratum].pixels += row.pixels();
            if row.pixels() >= LARGE_IMAGE_PIXELS {
                candidate_large_count += 1;
            }
            let folder = folders
                .entry(row.folder_id)
                .or_insert_with(|| FolderTotals {
                    path: row.folder_path.clone(),
                    ..FolderTotals::default()
                });
            folder.count += 1;
            folder.bytes += bytes;
            folder.stratum_bytes[stratum] += bytes;
            strata_entries[stratum].push(StratumEntry {
                id: row.id,
                bytes,
                pixels: row.pixels(),
            });
        }
        reporter.percent(scan_percentage(scanned, scan_total));
    }

    let candidate_count: u64 = totals.iter().map(|stratum| stratum.count).sum();
    let candidate_bytes: u64 = totals.iter().map(|stratum| stratum.bytes).sum();
    let candidate_pixels: u64 = totals.iter().map(|stratum| stratum.pixels).sum();
    let candidate_fingerprint = {
        let ids: Vec<i64> = strata_entries
            .iter()
            .flatten()
            .map(|entry| entry.id)
            .collect();
        crate::library::optimize::fingerprint_ids(&ids)
    };
    if candidate_count == 0 {
        finish_progress(reporter);
        return Ok(Some(empty_estimate(
            task_id,
            options_fingerprint,
            total_count,
            total_bytes,
            &skips,
        )));
    }

    let allocation = allocate_samples(&totals, sample_budget);
    let mut rng = Rng::new(seed());
    let mut sample_tasks: Vec<SampleTask> = Vec::new();
    for (stratum, &take) in allocation.iter().enumerate() {
        if take == 0 || strata_entries[stratum].is_empty() {
            continue;
        }
        // 行已被删除时用该层平均像素兜底，只影响失败样本的耗时外推。
        let average_pixels = totals[stratum].pixels / totals[stratum].count.max(1);
        for (index, weight_bytes, weight_count) in
            slice_indices(&strata_entries[stratum], take, &mut rng)
        {
            let entry = strata_entries[stratum][index];
            sample_tasks.push(SampleTask {
                stratum,
                id: entry.id,
                db_pixels: if entry.pixels > 0 {
                    entry.pixels
                } else {
                    average_pixels
                },
                weight_bytes,
                weight_count,
            });
        }
    }
    let sample_total = sample_tasks.len();
    reporter.message("正在挑选样本…");
    reporter.report(0, sample_total);
    reporter.percent(SAMPLE_PERCENT_START);
    if sample_total == 0 {
        finish_progress(reporter);
        return Ok(Some(empty_estimate(
            task_id,
            options_fingerprint,
            total_count,
            total_bytes,
            &skips,
        )));
    }

    let sample_ids: Vec<i64> = sample_tasks.iter().map(|task| task.id).collect();
    let sample_rows: HashMap<i64, CandidateRow> = collect_candidates(database, &sample_ids)?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();

    let pool = images.pool("估算")?;
    let threads = pool.current_num_threads();

    reporter.message("正在编码样本…");
    let done = AtomicUsize::new(0);
    let started = Instant::now();
    let records = pool.install(|| {
        sample_tasks
            .par_iter()
            .filter_map(|task| {
                if reporter.cancelled() {
                    return None;
                }
                let row = sample_rows.get(&task.id);
                let (outcome, error) = match row {
                    Some(row) => {
                        let outcome = sample_one(
                            database,
                            root,
                            row,
                            options,
                            resources,
                            images,
                            memory,
                            holder,
                            &|| reporter.cancelled(),
                        );
                        let error = outcome.error.clone();
                        (outcome, error)
                    }
                    None => (
                        failed_outcome(task.db_pixels, 0, 0.0, "图片行已被删除".to_owned()),
                        Some("图片行已被删除".to_owned()),
                    ),
                };
                // 切片权重由抽样阶段决定，这里回填给后面的外推与区间重抽。
                let mut outcome = outcome;
                outcome.weight_bytes = task.weight_bytes;
                outcome.weight_count = task.weight_count;
                let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                reporter.report(completed, sample_total);
                reporter.percent(sample_percentage(completed, sample_total));
                Some((task.stratum, task.id, outcome, error))
            })
            .collect::<Vec<_>>()
    });
    let elapsed = started.elapsed().as_secs_f64();
    if reporter.cancelled() {
        return Ok(None);
    }

    // 样本编码完还有汇总、外推与构造结果；先把进度推满并换文案，避免看起来卡在最后一张。
    reporter.report(sample_total, sample_total);
    finish_progress(reporter);

    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    let mut sample_bytes = 0_u64;
    let mut sample_large_count = 0_u64;
    let mut read_secs = 0.0_f64;
    let mut sample_failures = Vec::new();
    let mut bad_pixel_sum = 0.0_f64;
    let mut bad_pixel_count = 0_u64;
    for (stratum, id, outcome, error) in records {
        sample_bytes += outcome.old_bytes;
        if outcome.source_pixels >= LARGE_IMAGE_PIXELS {
            sample_large_count += 1;
        }
        read_secs += outcome.read_secs;
        if outcome.quality.is_some() {
            bad_pixel_sum += outcome.bad_pixel_ratio;
            bad_pixel_count += 1;
        }
        if let Some(error) = error
            && sample_failures.len() < MAX_SAMPLE_FAILURES
        {
            sample_failures.push(format!("图片 #{id}：{error}"));
        }
        samples[stratum].push(outcome);
    }

    let savings_bytes = point_savings(&totals, &samples);
    let mut skip_totals = cumulative_skips(&totals, &samples);
    // 外推后的各桶可能略超候选总量；先收窄再算 process，漏斗才不会出现负数或重复。
    skip_totals.fit_within(candidate_count, candidate_bytes);
    let process_count = candidate_count - skip_totals.count();
    let process_bytes = candidate_bytes - skip_totals.bytes();
    let predicted_bytes = total_bytes.saturating_sub(savings_bytes);

    let mut quality_counts: HashMap<u8, u64> = HashMap::new();
    for stratum in &samples {
        for sample in stratum {
            if let Some(quality) = sample.quality {
                *quality_counts.entry(quality).or_default() += 1;
            }
        }
    }
    let mut quality_counts: Vec<QualityCount> = quality_counts
        .into_iter()
        .map(|(quality, count)| QualityCount { quality, count })
        .collect();
    quality_counts.sort_by_key(|entry| entry.quality);

    let rates = stratum_rates(&totals, &samples);
    let mut folder_estimates: Vec<FolderEstimate> = folders
        .into_iter()
        .map(|(folder_id, folder)| {
            let savings: u64 = folder
                .stratum_bytes
                .iter()
                .zip(rates.iter())
                .map(|(bytes, rate)| (*bytes as f64 * rate).round() as u64)
                .sum();
            FolderEstimate {
                folder_id,
                path: folder.path,
                count: folder.count,
                bytes: folder.bytes,
                savings_bytes: savings.min(folder.bytes),
            }
        })
        .collect();
    folder_estimates.sort_by(|left, right| {
        right
            .savings_bytes
            .cmp(&left.savings_bytes)
            .then_with(|| right.bytes.cmp(&left.bytes))
    });

    let calibration = OptimizeCalibration::load(database)?.unwrap_or(CONSERVATIVE_CALIBRATION);
    let inputs = TimingInputs {
        read_secs,
        threads,
        sample_bytes,
        candidate_pixels,
        candidate_bytes,
    };
    // 点估计用实测墙钟，比逐张累加更准；放大倍率仍按各层自己的像素比算。
    let measured = WorkTotals {
        total_secs: elapsed,
        ..WorkTotals::collect(&totals, &samples)
    };
    let timing = estimate_seconds(
        &inputs,
        &measured,
        process_count,
        process_bytes,
        savings_bytes,
        calibration,
    );
    let margins = margins(
        &totals,
        &samples,
        BOOTSTRAP_ROUNDS,
        &inputs,
        timing,
        candidate_count,
        sample_total as u64,
    );

    Ok(Some(OptimizeEstimate {
        task_id: task_id.to_owned(),
        created_at: now_ms(),
        options_fingerprint: options_fingerprint.to_owned(),
        candidate_fingerprint,
        options: Some(options.clone()),
        // 范围与图库标识由 `AppState` 在写入缓存前补齐。
        scope: None,
        scope_label: String::new(),
        // 记录所选档位（而不是实际抽到的张数），界面据此判断候选是否不足。
        sample_size: sample_budget as u64,
        library_id: String::new(),
        sample_count: sample_total as u64,
        sample_bytes,
        coverage: sample_bytes as f64 / candidate_bytes.max(1) as f64,
        sample_large_count,
        candidate_large_count,
        sample_elapsed_ms: (elapsed * 1000.0).round() as u64,
        sample_bad_pixel_ratio: if bad_pixel_count > 0 {
            bad_pixel_sum / bad_pixel_count as f64
        } else {
            0.0
        },
        total_count,
        total_bytes,
        candidate_count,
        candidate_bytes,
        process_count,
        process_bytes,
        skip_compact: skips.compact.clone(),
        skip_optimized: skips.optimized.clone(),
        skip_transparent: skips.transparent.clone(),
        skip_too_large: skips.too_large.clone(),
        skip_animated: skips.animated.clone(),
        skip_color_profile: skip_totals.color_profile,
        skip_depth: skip_totals.depth,
        skip_quality: skip_totals.quality,
        skip_not_smaller: skip_totals.not_smaller,
        skip_multiframe: skip_totals.multiframe,
        skip_late: skip_totals.late,
        failed_count: skip_totals.failed_count,
        failed_bytes: skip_totals.failed_bytes,
        savings_bytes,
        savings_margin_bytes: margins.savings,
        predicted_bytes,
        estimated_seconds: timing.total(),
        estimated_seconds_margin: margins.seconds,
        estimated_work_seconds: timing.work,
        estimated_write_seconds: timing.write,
        estimated_finish_seconds: timing.finish,
        quality_counts,
        curve: benefit_curve(&totals, &samples, timing.work, timing.write, timing.finish),
        folders: folder_estimates,
        sample_failures,
    }))
}

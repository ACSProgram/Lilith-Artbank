use std::{fs, io::Cursor};

use image::{DynamicImage, Rgb, RgbImage};
use rusqlite::params;
use tempfile::tempdir;

use super::extrapolate::{
    TimeBreakdown, TimingInputs, benefit_curve, cumulative_skips, fit_to_total, margins,
    point_savings,
};
use super::report::SkipTotals;
use super::sampling::{
    MIN_PER_STRATUM, Rng, SAMPLE_PERCENT_START, SCAN_PERCENT, SampleOutcome, StratumEntry,
    allocate_samples, sample_percentage, scan_percentage, slice_indices,
};
use super::strata::{STRATUM_COUNT, StratumTotals, stratum_rates};
use super::*;
use crate::imageops::optimize::{OptimizeOptions, SkipReason};
use crate::library::{BatchReporter, service};

#[derive(Default)]
struct Recorder {
    completed: std::sync::atomic::AtomicUsize,
    percent: std::sync::atomic::AtomicU8,
    messages: std::sync::Mutex<Vec<String>>,
}

impl BatchReporter for Recorder {
    fn report(&self, completed: usize, _total: usize) {
        self.completed
            .store(completed, std::sync::atomic::Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        false
    }

    fn percent(&self, percent: u8) {
        self.percent
            .store(percent, std::sync::atomic::Ordering::Relaxed);
    }

    fn message(&self, message: &str) {
        self.messages.lock().unwrap().push(message.to_owned());
    }
}

fn totals(count: u64, bytes: u64) -> StratumTotals {
    StratumTotals {
        count,
        bytes,
        pixels: count * 1_000_000,
    }
}

#[test]
fn sample_size_accepts_only_range_and_step_multiples() {
    assert!(sample_size_valid(50));
    assert!(sample_size_valid(200));
    assert!(sample_size_valid(2000));
    assert!(!sample_size_valid(49));
    assert!(!sample_size_valid(55));
    assert!(!sample_size_valid(2050));
}

#[test]
fn progress_percentages_stay_inside_their_phase() {
    // 统计阶段只走到 25%，越界（含已删除的行）也不会越过它。
    assert_eq!(scan_percentage(0, 390_542), 0);
    assert_eq!(scan_percentage(390_542, 390_542), SCAN_PERCENT);
    assert_eq!(scan_percentage(400_000, 390_542), SCAN_PERCENT);
    // 编码样本从 26% 起步，正好在最后一张到 100%。
    assert_eq!(sample_percentage(0, 200), SAMPLE_PERCENT_START);
    assert_eq!(sample_percentage(200, 200), 100);
    assert!(sample_percentage(100, 200) > scan_percentage(390_542, 390_542));
    assert_eq!(sample_percentage(0, 0), 100);
}

#[test]
fn allocates_by_volume_with_a_floor_per_stratum() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    strata[0] = totals(1000, 1_000_000);
    strata[1] = totals(1000, 100_000);
    strata[2] = totals(1000, 10_000);
    let allocation = allocate_samples(&strata, 30);
    assert_eq!(allocation.iter().sum::<usize>(), 30);
    assert!(allocation[0] >= MIN_PER_STRATUM);
    assert!(allocation[1] >= MIN_PER_STRATUM);
    assert!(allocation[2] >= MIN_PER_STRATUM);
    assert!(allocation[0] > allocation[1]);
    assert!(allocation[1] > allocation[2]);
}

#[test]
fn allocation_never_exceeds_stratum_size() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    strata[0] = totals(2, 1_000_000);
    strata[1] = totals(1000, 1_000);
    let allocation = allocate_samples(&strata, 100);
    assert_eq!(allocation[0], 2);
    assert!(allocation[1] <= 1000);
    assert_eq!(allocation.iter().sum::<usize>(), 100);
}

#[test]
fn extrapolates_savings_and_skips_from_samples() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    strata[0] = totals(100, 10_000);
    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    let outcome = |new_bytes: u64, quality: Option<u8>, skip: Option<SkipReason>, failed: bool| {
        SampleOutcome {
            old_bytes: 100,
            new_bytes,
            // 四份等体积切片：每份代表 25 张 / 2 500 字节。
            weight_bytes: 2_500,
            weight_count: 25,
            quality,
            skip,
            failed,
            error: None,
            source_pixels: 1_000,
            read_secs: 0.0,
            encode_secs: 0.0,
            hash_secs: 0.0,
            recalc_secs: 0.0,
            bad_pixel_ratio: 0.0,
        }
    };
    samples[0] = vec![
        outcome(50, Some(85), None, false),
        outcome(50, Some(85), None, false),
        outcome(100, None, Some(SkipReason::QualityNotMet), false),
        outcome(100, None, None, true),
    ];
    assert_eq!(point_savings(&strata, &samples), 2_500);
    let skips = cumulative_skips(&strata, &samples);
    assert_eq!(skips.color_profile.count, 0);
    assert_eq!(skips.depth.count, 0);
    assert_eq!(skips.quality.count, 25);
    assert_eq!(skips.not_smaller.count, 0);
    assert_eq!(skips.multiframe.count, 0);
    assert_eq!(skips.late.count, 0);
    assert_eq!(skips.failed_count, 25);
    assert_eq!(skips.failed_bytes, 2_500);
    // 候选 100 张 = 处理 50 + 跳过 50，漏斗闭合。
    assert_eq!(skips.count() + 50, 100);
}

/// 切片权重与“按张数等分”在均匀数据上必须一致：防止权重公式写反。
#[test]
fn slice_weights_match_count_scaling_on_uniform_data() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    strata[0] = totals(40, 4_000);
    let entries: Vec<StratumEntry> = (0..40)
        .map(|index| StratumEntry {
            id: index,
            bytes: 100,
            pixels: 1_000,
        })
        .collect();
    let mut rng = Rng::new(7);
    let slices = slice_indices(&entries, 4, &mut rng);
    assert_eq!(slices.len(), 4);
    // 每份 10 张、1 000 字节，与按张数等分完全一致。
    assert!(
        slices
            .iter()
            .all(|(_, bytes, count)| *bytes == 1_000 && *count == 10)
    );
    assert_eq!(slices.iter().map(|(_, bytes, _)| bytes).sum::<u64>(), 4_000);
    assert_eq!(slices.iter().map(|(_, _, count)| count).sum::<u64>(), 40);

    let samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|index| {
        if index != 0 {
            return Vec::new();
        }
        slices
            .iter()
            .enumerate()
            .map(|(position, (_, bytes, count))| SampleOutcome {
                old_bytes: 100,
                new_bytes: if position == 0 { 50 } else { 90 },
                weight_bytes: *bytes,
                weight_count: *count,
                quality: Some(85),
                skip: None,
                failed: false,
                error: None,
                source_pixels: 1_000,
                read_secs: 0.0,
                encode_secs: 0.0,
                hash_secs: 0.0,
                recalc_secs: 0.0,
                bad_pixel_ratio: 0.0,
            })
            .collect()
    });
    // 均匀数据上，切片字节加权 = 按样本旧字节加权，两种口径必须给出同一个率。
    let old: u64 = samples[0].iter().map(|sample| sample.old_bytes).sum();
    let new: u64 = samples[0].iter().map(|sample| sample.new_bytes).sum();
    let expected = 1.0 - new as f64 / old as f64;
    let rates = stratum_rates(&strata, &samples);
    assert!((rates[0] - expected).abs() < 1e-9, "{:?}", rates[0]);
}

/// 重尾分布下，切片抽样必须让样本的体积构成贴合总体；这是本批次的核心修复。
#[test]
fn slice_sampling_keeps_volume_share_of_heavy_tail() {
    // 1 张占 90% 体积的大图 + 999 张小图，只抽 10 张。
    let mut entries: Vec<StratumEntry> = (0..999)
        .map(|index| StratumEntry {
            id: index,
            bytes: 1_000,
            pixels: 100_000,
        })
        .collect();
    entries.push(StratumEntry {
        id: 999,
        bytes: 9_000_000,
        pixels: 40_000_000,
    });
    let total: u64 = entries.iter().map(|entry| entry.bytes).sum();
    let mut rng = Rng::new(11);
    let slices = slice_indices(&entries, 10, &mut rng);
    // 大图独占若干份时会合成一份，所以份数只保证不超过档位。
    assert!(slices.len() <= 10, "{slices:?}");
    // 每张只被选一次。
    let mut picked: Vec<usize> = slices.iter().map(|(index, _, _)| *index).collect();
    picked.sort_unstable();
    picked.dedup();
    assert_eq!(picked.len(), slices.len(), "{slices:?}");
    // 大图所在那一份必然把大图抽出来：尾部不会再整体缺席。
    let big = slices.iter().find(|(index, _, _)| *index == 999);
    assert!(big.is_some(), "大图没有被抽到：{slices:?}");
    // 权重之和等于总体，外推不会凭空放大或缩小。
    assert_eq!(slices.iter().map(|(_, bytes, _)| bytes).sum::<u64>(), total);
    assert_eq!(slices.iter().map(|(_, _, count)| count).sum::<u64>(), 1_000);
}

/// 抽样就是全体时，两个区间都必须是 0。
#[test]
fn margins_vanish_when_sampling_is_complete() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    strata[0] = totals(2, 2_000);
    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    for index in 0..2_u64 {
        samples[0].push(SampleOutcome {
            old_bytes: 1_000,
            new_bytes: 500 + index * 100,
            weight_bytes: 1_000,
            weight_count: 1,
            quality: Some(85),
            skip: None,
            failed: false,
            error: None,
            source_pixels: 1_000,
            read_secs: 0.01,
            encode_secs: 0.05,
            hash_secs: 0.0,
            recalc_secs: 0.0,
            bad_pixel_ratio: 0.0,
        });
    }
    let inputs = TimingInputs {
        read_secs: 0.02,
        threads: 8,
        sample_bytes: 2_000,
        candidate_pixels: 2_000,
        candidate_bytes: 2_000,
    };
    let timing = TimeBreakdown {
        work: 1,
        write: 4,
        finish: 2,
    };
    let complete = margins(&strata, &samples, 50, &inputs, timing, 2, 2);
    assert_eq!((complete.savings, complete.seconds), (0, 0));
    // 只抽到一半时才有区间。
    let partial = margins(&strata, &samples, 50, &inputs, timing, 4, 2);
    assert!(partial.savings > 0, "{partial:?}");
    assert!(partial.seconds > 0, "{partial:?}");
}

/// 区间重抽要把逐张累加的串行耗时折算成墙钟：否则耗时段会凭空多出一个线程数倍。
#[test]
fn seconds_margin_normalizes_serial_time_to_wall() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    strata[0] = totals(100, 100_000);
    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    // 每像素成本差 10 倍：重抽才有可测的波动。
    for index in 0..10_u64 {
        samples[0].push(SampleOutcome {
            old_bytes: 1_000,
            new_bytes: 400 + index * 20,
            weight_bytes: 10_000,
            weight_count: 10,
            quality: Some(85),
            skip: None,
            failed: false,
            error: None,
            source_pixels: 1_000,
            read_secs: 0.01,
            encode_secs: 0.02 * (index + 1) as f64,
            hash_secs: 0.0,
            recalc_secs: 0.0,
            bad_pixel_ratio: 0.0,
        });
    }
    let make = |threads: usize| TimingInputs {
        read_secs: 0.1,
        threads,
        sample_bytes: 10_000,
        candidate_pixels: 100_000,
        candidate_bytes: 100_000,
    };
    let timing = TimeBreakdown {
        work: 10,
        write: 0,
        finish: 0,
    };
    let single = margins(&strata, &samples, 120, &make(1), timing, 100, 10);
    let parallel = margins(&strata, &samples, 120, &make(8), timing, 100, 10);
    assert!(single.seconds > 0, "{single:?}");
    // 折算后耗时段随线程数成比例收窄；没折算时两者会一样宽。
    assert!(
        parallel.seconds * 4 < single.seconds,
        "single={single:?} parallel={parallel:?}"
    );
}

#[test]
fn curve_accumulates_savings_in_bpp_order() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    strata[0] = totals(100, 1_000);
    strata[1] = totals(100, 3_000);
    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    let sample = |new_bytes: u64, weight_bytes: u64| SampleOutcome {
        old_bytes: 100,
        new_bytes,
        weight_bytes,
        weight_count: 100,
        quality: Some(85),
        skip: None,
        failed: false,
        error: None,
        source_pixels: 1_000,
        read_secs: 0.0,
        encode_secs: 0.0,
        hash_secs: 0.0,
        recalc_secs: 0.0,
        bad_pixel_ratio: 0.0,
    };
    samples[0].push(sample(50, 1_000));
    samples[1].push(sample(90, 3_000));
    // 三段耗时都只由像素/输出字节/张数决定，这里给 60/30/10 便于断言。
    let curve = benefit_curve(&strata, &samples, 60, 30, 10);
    assert_eq!(curve.len(), 3);
    // 横轴是张数：两层各 100 张，走完第一层是 50%，不是体积的 75%。
    assert_eq!(curve[1].coverage, 0.5);
    assert_eq!(curve[1].savings_bytes, 300);
    assert_eq!(curve[2].coverage, 1.0);
    assert_eq!(curve[2].savings_bytes, 800);
    // 最后一点的耗时就是整次执行的总预计耗时。
    assert_eq!(curve[2].elapsed_seconds, 100);
    assert!(curve[1].elapsed_seconds > 0 && curve[1].elapsed_seconds < 100);
}

/// 横轴按张数累计：体积分布极不均匀时也走满 100%，中途点等于张数份额。
#[test]
fn curve_axis_accumulates_counts_not_bytes() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    // 高层只有 10 张却占 900/1000 的体积，正是实测里「35% 张数占 72% 体积」的极端版。
    strata[0] = totals(10, 900);
    strata[1] = totals(90, 100);
    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    let sample = |old: u64, new: u64, weight_bytes: u64, weight_count: u64| SampleOutcome {
        old_bytes: old,
        new_bytes: new,
        weight_bytes,
        weight_count,
        quality: Some(85),
        skip: None,
        failed: false,
        error: None,
        source_pixels: 1_000,
        read_secs: 0.0,
        encode_secs: 0.0,
        hash_secs: 0.0,
        recalc_secs: 0.0,
        bad_pixel_ratio: 0.0,
    };
    samples[0].push(sample(900, 400, 900, 10));
    samples[1].push(sample(100, 95, 100, 90));
    let curve = benefit_curve(&strata, &samples, 0, 0, 0);
    assert_eq!(curve.len(), 3);
    // 先走完低 bpp 那层：90 张 / 100 张 = 90%，按体积只有 10%。
    assert!((curve[1].coverage - 0.9).abs() < 1e-9, "{:?}", curve[1]);
    assert_eq!(curve[2].coverage, 1.0);
    // 末点就是全部节省。
    assert_eq!(curve[2].savings_bytes, 500 + 5);
}

#[test]
fn savings_rate_is_not_capped_by_replace_threshold() {
    let mut strata: [StratumTotals; STRATUM_COUNT] = std::array::from_fn(|_| Default::default());
    strata[0] = totals(1, 1_000);
    let mut samples: [Vec<SampleOutcome>; STRATUM_COUNT] = std::array::from_fn(|_| Vec::new());
    samples[0].push(SampleOutcome {
        old_bytes: 1_000,
        new_bytes: 20,
        weight_bytes: 1_000,
        weight_count: 1,
        quality: Some(75),
        skip: None,
        failed: false,
        error: None,
        source_pixels: 1_000,
        read_secs: 0.0,
        encode_secs: 0.0,
        hash_secs: 0.0,
        recalc_secs: 0.0,
        bad_pixel_ratio: 0.0,
    });
    let rates = stratum_rates(&strata, &samples);
    // 压到原图 2% 时节省率应是 98%，不能被 0.95 的替换阈值夹住。
    assert!((rates[0] - 0.98).abs() < 1e-9, "{:?}", rates[0]);
}

#[test]
fn skip_buckets_fit_within_candidate_totals() {
    let mut skips = SkipTotals {
        quality: SkipBucket {
            count: 80,
            bytes: 800,
        },
        multiframe: SkipBucket {
            count: 40,
            bytes: 400,
        },
        failed_count: 30,
        failed_bytes: 300,
        ..SkipTotals::default()
    };
    // 外推后各桶之和（150 张）超过候选（100 张），收窄到候选以内。
    skips.fit_within(100, 1_000);
    assert_eq!(skips.count(), 100);
    assert_eq!(skips.bytes(), 1_000);
}

#[test]
fn fit_to_total_keeps_zero_and_small_sets() {
    let mut values = [1_u64, 2, 3];
    fit_to_total(&mut values, 100);
    assert_eq!(values, [1, 2, 3]);
    let mut values = [1_u64, 2, 3];
    fit_to_total(&mut values, 0);
    assert_eq!(values, [0, 0, 0]);
    let mut values = [10_u64, 0, 0];
    fit_to_total(&mut values, 4);
    assert_eq!(values.iter().sum::<u64>(), 4);
    assert_eq!(values[0], 4);
}

/// 一条估算里同时出现前置跳过、候选内跳过、动画/多页与失败，
/// 四个不变量必须严格成立。
#[test]
fn funnel_invariants_hold_with_animated_and_failed_samples() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "漏斗测试", 1).unwrap();

    let insert = |format: &str, ext: &str, bytes: &[u8], width: u32, height: u32| {
        let hash = blake3::hash(bytes).to_hex().to_string();
        if !bytes.is_empty() {
            fs::write(root.join(format!("{hash}.{ext}")), bytes).unwrap();
        }
        session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 2, '', 1, 1)",
                    params![hash, ext, bytes.len() as i64, width, height, format],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap()
    };

    let mut ids = Vec::new();
    // 三张正常 PNG：进候选。
    for index in 0..3 {
        let mut image = RgbImage::new(64, 64);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let value = ((x * 31 + y * 17 + index * 7) % 256) as u8;
            *pixel = Rgb([value, value.wrapping_mul(3), value.wrapping_mul(5)]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        ids.push(insert("png", "png", &bytes, 64, 64));
    }
    // GIF：只看数据库列就跳过，不需要真实文件。
    ids.push(insert("gif", "gif", &[], 64, 64));
    // 假 APNG：带 acTL，候选内跳过。
    let mut apng = b"\x89PNG\r\n\x1a\n".to_vec();
    apng.extend_from_slice(&0_u32.to_be_bytes());
    apng.extend_from_slice(b"acTL");
    apng.extend_from_slice(&[0, 0, 0, 0]);
    apng.resize(256, 0);
    ids.push(insert("png", "png", &apng, 64, 64));
    // 行还在但文件缺失：读取失败。
    ids.push(insert("png", "png", &[7_u8; 256], 64, 64));

    let reporter = Recorder::default();
    let memory = crate::concurrency::budget::ImageMemoryBudget::new(1);
    let estimate = estimate(
        &session.db,
        &root,
        "funnel",
        &ids,
        &OptimizeOptions::default(),
        50,
        "fingerprint",
        &reporter,
        &crate::concurrency::resources::ResourceTable::new(),
        &crate::concurrency::budget::ImageBudget::new(2),
        &memory,
        &crate::concurrency::resources::HolderInfo::new("estimate-test", "估算"),
    )
    .unwrap()
    .unwrap();
    // 额度小到每张都超限，样本编码走独占路径；结束后额度必须全部归还。
    assert_eq!(memory.in_use(), 0);

    let prefilter_count = estimate.skip_compact.count
        + estimate.skip_optimized.count
        + estimate.skip_transparent.count
        + estimate.skip_too_large.count
        + estimate.skip_animated.count;
    let prefilter_bytes = estimate.skip_compact.bytes
        + estimate.skip_optimized.bytes
        + estimate.skip_transparent.bytes
        + estimate.skip_too_large.bytes
        + estimate.skip_animated.bytes;
    let candidate_skip_count = estimate.skip_color_profile.count
        + estimate.skip_depth.count
        + estimate.skip_quality.count
        + estimate.skip_not_smaller.count
        + estimate.skip_multiframe.count
        + estimate.skip_late.count
        + estimate.failed_count;
    let candidate_skip_bytes = estimate.skip_color_profile.bytes
        + estimate.skip_depth.bytes
        + estimate.skip_quality.bytes
        + estimate.skip_not_smaller.bytes
        + estimate.skip_multiframe.bytes
        + estimate.skip_late.bytes
        + estimate.failed_bytes;

    assert_eq!(
        estimate.total_count,
        prefilter_count + estimate.candidate_count,
        "{estimate:?}"
    );
    assert_eq!(
        estimate.total_bytes,
        prefilter_bytes + estimate.candidate_bytes,
        "{estimate:?}"
    );
    assert_eq!(
        estimate.candidate_count,
        estimate.process_count + candidate_skip_count,
        "{estimate:?}"
    );
    assert_eq!(
        estimate.candidate_bytes,
        estimate.process_bytes + candidate_skip_bytes,
        "{estimate:?}"
    );

    // GIF 进前置桶，APNG 与失败进候选内桶。
    assert_eq!(estimate.skip_animated.count, 1);
    assert_eq!(estimate.skip_multiframe.count, 1);
    assert_eq!(estimate.failed_count, 1);
    // 档位 50 > 候选 5，结果里记的是档位而不是实际张数。
    assert_eq!(estimate.sample_size, 50);
    assert_eq!(estimate.sample_count, 5);
    assert!(estimate.sample_elapsed_ms > 0 || estimate.sample_count == 0);
    assert_eq!(
        estimate.estimated_seconds,
        estimate.estimated_work_seconds
            + estimate.estimated_write_seconds
            + estimate.estimated_finish_seconds
    );
}

#[test]
fn estimate_is_read_only() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "估算测试", 1).unwrap();
    let mut ids = Vec::new();
    let mut original_hashes = Vec::new();
    for index in 0..3 {
        let mut image = RgbImage::new(64, 64);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let value = ((x * 31 + y * 17 + index * 7) % 256) as u8;
            *pixel = Rgb([value, value.wrapping_mul(3), value.wrapping_mul(5)]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        fs::write(root.join(format!("{hash}.png")), &bytes).unwrap();
        let id = session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                            content_hash, file_ext, folder_id, file_size, width, height, format,
                            has_alpha, phash, colors4, colors8, score, description, imported_at,
                            modified_at
                         ) VALUES (?1, 'png', 1, ?2, 64, 64, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 2, '保留', 1, 1)",
                    params![hash, bytes.len() as i64],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap();
        ids.push(id);
        original_hashes.push((hash, bytes.len() as i64));
    }

    let before: Vec<(i64, String, i64, i64)> = session
        .db
        .with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, content_hash, file_size, modified_at FROM images ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();

    let options = OptimizeOptions::default();
    let reporter = Recorder::default();
    let estimate = estimate(
        &session.db,
        &root,
        "estimate-test",
        &ids,
        &options,
        10,
        "fingerprint",
        &reporter,
        &crate::concurrency::resources::ResourceTable::new(),
        &crate::concurrency::budget::ImageBudget::new(2),
        &crate::concurrency::budget::ImageMemoryBudget::default(),
        &crate::concurrency::resources::HolderInfo::new("estimate-test", "估算"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(estimate.total_count, 3);
    assert_eq!(estimate.candidate_count, 3);
    assert!(estimate.sample_count > 0);
    assert_eq!(
        reporter.percent.load(std::sync::atomic::Ordering::Relaxed),
        100,
        "估算结束时进度应到 100%"
    );

    let after: Vec<(i64, String, i64, i64)> = session
        .db
        .with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, content_hash, file_size, modified_at FROM images ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(before, after);
    let operations: i64 = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row("SELECT count(*) FROM operations", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(operations, 0);
    let temp_dir = root.join(".artbank").join("temp");
    assert_eq!(fs::read_dir(temp_dir).unwrap().count(), 0);
    for (hash, size) in original_hashes {
        let path = root.join(format!("{hash}.png"));
        assert_eq!(fs::metadata(path).unwrap().len(), size as u64);
    }
}

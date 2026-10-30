use std::{collections::HashSet, path::Path};

use rayon::prelude::*;

use crate::{
    app::error::{AppError, AppResult},
    imageops::optimize::{self, OptimizeOptions, SourceInfo},
    library::{db::LibraryDb, metadata, optimize_estimate::OptimizeEstimate, paths::LibraryPaths},
};

pub(super) const CHUNK: usize = 400;
/// 执行前逐张 `stat` 原图的线程数；热缓存下约 19 µs/张，冷盘会明显更慢。
const SOURCE_CHECK_THREADS: usize = 8;

#[derive(Debug, Clone)]
pub struct CandidateRow {
    pub id: i64,
    pub content_hash: String,
    pub file_ext: String,
    pub folder_id: i64,
    pub folder_path: String,
    pub file_size: i64,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub has_alpha: bool,
    pub optimized_at: i64,
}

impl CandidateRow {
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    pub fn bpp(&self) -> f64 {
        let pixels = self.pixels();
        if pixels == 0 {
            return 0.0;
        }
        self.file_size.max(0) as f64 * 8.0 / pixels as f64
    }

    pub fn lossless_source(&self) -> bool {
        !matches!(self.format.as_str(), "jpeg" | "jpg" | "webp")
    }

    /// 只依赖数据库列的源信息；位深按 8 位占位，解码后再用真实值覆盖。
    pub fn source_info(&self) -> SourceInfo {
        SourceInfo {
            format: self.format.clone(),
            file_size: self.file_size,
            width: self.width,
            height: self.height,
            has_alpha: self.has_alpha,
            optimized_at: self.optimized_at,
            bit_depth: 8,
        }
    }
}

/// 按 id 分块读候选行；只取未删除的图片。
pub fn collect_candidates(database: &LibraryDb, ids: &[i64]) -> AppResult<Vec<CandidateRow>> {
    let mut rows = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let sql = format!(
            "SELECT images.id, images.content_hash, images.file_ext, images.folder_id,
                    folders.path, images.file_size, images.width, images.height,
                    images.format, images.has_alpha, images.optimized_at
             FROM images JOIN folders ON folders.id = images.folder_id
             WHERE images.deleted_at IS NULL
               AND images.id IN ({})",
            crate::library::placeholders(chunk.len())
        );
        let mut part = database.with_reader(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok(CandidateRow {
                        id: row.get(0)?,
                        content_hash: row.get(1)?,
                        file_ext: row.get(2)?,
                        folder_id: row.get(3)?,
                        folder_path: row.get(4)?,
                        file_size: row.get(5)?,
                        width: row.get(6)?,
                        height: row.get(7)?,
                        format: row.get(8)?,
                        has_alpha: row.get::<_, i64>(9)? != 0,
                        optimized_at: row.get(10)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.append(&mut part);
    }
    Ok(rows)
}

/// 前置判据之后的候选张数与字节数；用于和估算凭证比对。
pub fn eligible_summary(candidates: &[CandidateRow], options: &OptimizeOptions) -> (u64, u64) {
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    for row in candidates {
        if optimize::prefilter(&row.source_info(), options).is_ok() {
            count += 1;
            bytes += row.file_size.max(0) as u64;
        }
    }
    (count, bytes)
}

/// 候选集合指纹：对排好序的 id 折叠，用来发现“张数与体积都对、但换了一批图”。
pub fn fingerprint_ids(ids: &[i64]) -> String {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for id in sorted {
        hasher.update(&id.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// 当前候选集合的指纹；口径与估算一致，只算前置判据通过的图片。
pub fn candidate_fingerprint(candidates: &[CandidateRow], options: &OptimizeOptions) -> String {
    let ids: Vec<i64> = candidates
        .iter()
        .filter(|row| optimize::prefilter(&row.source_info(), options).is_ok())
        .map(|row| row.id)
        .collect();
    fingerprint_ids(&ids)
}

/// 执行凭证：指纹一致、候选集在估算值的 ±10% 以内，且集合本身没换。
pub fn verify_grant(
    estimate: &OptimizeEstimate,
    options_fingerprint: &str,
    candidate_count: u64,
    candidate_bytes: u64,
    candidate_fingerprint: &str,
) -> AppResult<()> {
    if estimate.options_fingerprint != options_fingerprint {
        return Err(AppError::message(
            "估算结果已过期：范围或参数已改变，请重新估算",
        ));
    }
    if estimate.candidate_count == 0 || candidate_count == 0 {
        return Err(AppError::message("当前范围没有可压缩的图片，请重新估算"));
    }
    if !within_drift(estimate.candidate_count, candidate_count)
        || !within_drift(estimate.candidate_bytes, candidate_bytes)
    {
        return Err(AppError::message(
            "候选图片已变化，估算不再可靠，请重新估算",
        ));
    }
    // 删掉一批又导入一批体积接近的图，张数与体积都能对上，只有集合指纹能发现。
    if !estimate.candidate_fingerprint.is_empty()
        && estimate.candidate_fingerprint != candidate_fingerprint
    {
        return Err(AppError::message(
            "候选图片已换成另一批（数量与体积接近，但不是同一批图），请重新估算",
        ));
    }
    Ok(())
}

/// 执行前逐张核对原图的结果。
#[derive(Debug, Default)]
pub struct SourceCheck {
    /// 文件不存在（或被移走、路径不可用）。
    pub missing: u64,
    /// 文件还在，但大小与数据库记录不一致（被外部替换或截断）。
    pub changed: u64,
    /// 有问题、要从候选里剔除的行。
    pub bad_ids: Vec<i64>,
    /// 候选涉及的、已经不存在的文件夹路径。
    pub missing_dirs: Vec<String>,
}

impl SourceCheck {
    pub fn drifted(&self) -> u64 {
        self.missing + self.changed
    }
}

/// 逐张 `stat` 候选原图并与 `images.file_size` 比对，同时确认文件夹还在。
///
/// 274 068 个候选在热缓存下约 5 s（8 线程），相对整次执行只占百分之几；串行要
/// 30 s 以上，不能放进执行路径。
pub fn verify_sources(
    root: &Path,
    candidates: &[CandidateRow],
    options: &OptimizeOptions,
) -> SourceCheck {
    let paths = LibraryPaths::new(root);
    let rows: Vec<&CandidateRow> = candidates
        .iter()
        .filter(|row| optimize::prefilter(&row.source_info(), options).is_ok())
        .collect();

    let mut folders: HashSet<&str> = HashSet::new();
    for row in &rows {
        folders.insert(row.folder_path.as_str());
    }
    let mut missing_dirs: Vec<String> = folders
        .into_iter()
        .filter(|folder| !paths.folder_dir(folder).is_dir())
        .map(str::to_owned)
        .collect();
    missing_dirs.sort();

    let check = |row: &CandidateRow| -> (bool, bool) {
        let path = match metadata::content_path(
            &paths,
            &row.folder_path,
            &row.content_hash,
            &row.file_ext,
        ) {
            Ok(path) => path,
            // 路径不合法说明行本身有问题，按缺失处理，交给重新估算。
            Err(_) => return (true, false),
        };
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => (false, meta.len() != row.file_size.max(0) as u64),
            _ => (true, false),
        }
    };
    let outcomes: Vec<(bool, bool)> =
        match crate::library::encode_pool_with(SOURCE_CHECK_THREADS, "原图校验") {
            Ok(pool) => pool.install(|| rows.par_iter().map(|row| check(row)).collect()),
            // 建不了池就串行跑：慢一点，但不能因此跳过校验。
            Err(_) => rows.iter().map(|row| check(row)).collect(),
        };

    let mut result = SourceCheck {
        missing_dirs,
        ..SourceCheck::default()
    };
    for (row, (missing, changed)) in rows.iter().zip(outcomes) {
        if missing {
            result.missing += 1;
            result.bad_ids.push(row.id);
        } else if changed {
            result.changed += 1;
            result.bad_ids.push(row.id);
        }
    }
    result
}

/// 实际值与估算值相差不超过 10%。
fn within_drift(expected: u64, actual: u64) -> bool {
    let expected = expected.max(1) as f64;
    (actual as f64 - expected).abs() <= expected * 0.10
}

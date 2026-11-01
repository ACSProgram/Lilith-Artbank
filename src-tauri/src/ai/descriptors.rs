//! SPP / MaxSim 描述子的 int8 落盘。
//!
//! 两条记录都由服务端在同一次图片塔前向里算好并量化；这里只校验维度、追加到
//! feature pack 并更新 `feature_blobs` 指针。量化 scale 固定在白化基里，不进记录。

use std::collections::HashMap;

use rayon::prelude::*;
use rusqlite::{OptionalExtension, params_from_iter, types::Value};

use crate::{
    app::error::{AppError, AppResult},
    library::{db::LibraryDb, placeholders},
};

use super::pack::{FeatureDtype, FeatureKind, FeatureStore, record_slice, write_pointer};

/// 两个描述子都按空格子/局部摊平；打分时按这里的形状切块。
fn blocks_of(kind: FeatureKind) -> (usize, usize) {
    match kind {
        FeatureKind::Spp => (
            (FeatureKind::SPP_GRID_SIDE * FeatureKind::SPP_GRID_SIDE) as usize,
            FeatureKind::SPP_DIM_PER_CELL as usize,
        ),
        FeatureKind::MaxSim => (
            FeatureKind::MAXSIM_LOCALS as usize,
            FeatureKind::MAXSIM_DIM_PER_LOCAL as usize,
        ),
        _ => (0, 0),
    }
}

fn block_norm(bytes: &[u8]) -> f32 {
    let mut sum = 0.0_f32;
    for byte in bytes {
        let value = *byte as i8 as f32;
        sum += value * value;
    }
    sum.sqrt()
}

fn block_dot(left: &[u8], right: &[u8]) -> f32 {
    let mut sum = 0.0_f32;
    for (a, b) in left.iter().zip(right.iter()) {
        sum += (*a as i8 as f32) * (*b as i8 as f32);
    }
    sum
}

fn cosines(left: &[u8], left_norm: f32, right: &[u8], right_norm: f32) -> f32 {
    if left_norm <= 1e-6 || right_norm <= 1e-6 {
        return 0.0;
    }
    block_dot(left, right) / (left_norm * right_norm)
}

/// 描述子的块数上限：SPP 的 64 格比 MaxSim 的 32 局部多，取它当定长上限。
///
/// 打分在逐张候选的热路径上，用定长数组而不是 `Vec`，每张省两次堆分配。
const MAX_BLOCKS: usize = (FeatureKind::SPP_GRID_SIDE * FeatureKind::SPP_GRID_SIDE) as usize;

/// 参考图的描述子：查询侧只准备一次，逐张候选只读目标包。
pub struct PreparedDescriptor {
    kind: FeatureKind,
    bytes: Vec<u8>,
    blocks: usize,
    block_dim: usize,
    /// 每个格子/局部在 int8 空间里的模长；scale 在余弦里抵消，不需要基文件。
    norms: Vec<f32>,
}

impl PreparedDescriptor {
    pub fn new(kind: FeatureKind, bytes: Vec<u8>) -> AppResult<Self> {
        let expected = kind
            .fixed_dim()
            .ok_or_else(|| AppError::message(format!("{} 不是语义描述子", kind.name())))?;
        if bytes.len() != expected as usize {
            return Err(AppError::message(format!(
                "{} 记录应为 {expected} 字节，实际 {}",
                kind.name(),
                bytes.len()
            )));
        }
        let (blocks, block_dim) = blocks_of(kind);
        debug_assert!(blocks <= MAX_BLOCKS, "描述子块数超过定长上限");
        let mut norms = Vec::with_capacity(blocks);
        for block in 0..blocks {
            let norm = block_norm(&bytes[block * block_dim..(block + 1) * block_dim]);
            if !norm.is_finite() || norm <= 1e-6 {
                return Err(AppError::message(format!(
                    "{} 描述子有零长度块，无法比较",
                    kind.name()
                )));
            }
            norms.push(norm);
        }
        Ok(Self {
            kind,
            bytes,
            blocks,
            block_dim,
            norms,
        })
    }

    /// SPP：同位置格子逐对余弦后取平均。
    /// MaxSim：双向匹配，两侧各自取最近邻再平均。
    pub fn score(&self, record: &[u8]) -> Option<f32> {
        let (blocks, block_dim) = (self.blocks, self.block_dim);
        if blocks == 0 || record.len() != blocks * block_dim {
            return None;
        }
        match self.kind {
            FeatureKind::Spp => {
                let mut total = 0.0_f32;
                for block in 0..blocks {
                    let start = block * block_dim;
                    let right = &record[start..start + block_dim];
                    let right_norm = block_norm(right);
                    if right_norm <= 1e-6 {
                        return None;
                    }
                    total += cosines(
                        &self.bytes[start..start + block_dim],
                        self.norms[block],
                        right,
                        right_norm,
                    );
                }
                Some(total / blocks as f32)
            }
            FeatureKind::MaxSim => {
                let mut norms = [0.0_f32; MAX_BLOCKS];
                for (block, slot) in norms.iter_mut().enumerate().take(blocks) {
                    let start = block * block_dim;
                    let norm = block_norm(&record[start..start + block_dim]);
                    if !norm.is_finite() || norm <= 1e-6 {
                        return None;
                    }
                    *slot = norm;
                }
                let mut best_right = [f32::NEG_INFINITY; MAX_BLOCKS];
                let mut forward = 0.0_f32;
                for left in 0..blocks {
                    let left_start = left * block_dim;
                    let mut best = f32::NEG_INFINITY;
                    for right in 0..blocks {
                        let right_start = right * block_dim;
                        let value = cosines(
                            &self.bytes[left_start..left_start + block_dim],
                            self.norms[left],
                            &record[right_start..right_start + block_dim],
                            norms[right],
                        );
                        best = best.max(value);
                        best_right[right] = best_right[right].max(value);
                    }
                    forward += best;
                }
                // `best_right` 的下标只到 blocks，其余格的负无穷不参与求和。
                let backward: f32 = best_right[..blocks].iter().sum();
                Some((forward / blocks as f32 + backward / blocks as f32) / 2.0)
            }
            _ => None,
        }
    }
}

/// 读取某张图片的描述子原始记录；没有记录或记录损坏时返回 `None`。
pub fn target(
    database: &LibraryDb,
    store: &FeatureStore,
    image_id: i64,
    kind: FeatureKind,
) -> AppResult<Option<(String, Vec<u8>)>> {
    let dim = kind
        .fixed_dim()
        .ok_or_else(|| AppError::message(format!("{} 不是语义描述子", kind.name())))?;
    let pointer = database.with_reader(|connection| {
        Ok(connection
            .query_row(
                "SELECT feature_blobs.model, feature_blobs.pack_name,
                        feature_blobs.pack_offset, feature_blobs.pack_length
                 FROM feature_blobs
                 JOIN images ON images.id = feature_blobs.image_id
                 WHERE feature_blobs.image_id = ?1
                   AND feature_blobs.kind = ?2
                   AND feature_blobs.dim = ?3
                   AND images.deleted_at IS NULL",
                rusqlite::params![image_id, kind.name(), dim],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as u64,
                        row.get::<_, u32>(3)?,
                    ))
                },
            )
            .optional()?)
    })?;
    let Some((model, pack_name, offset, length)) = pointer else {
        return Ok(None);
    };
    if length != dim {
        return Ok(None);
    }
    Ok(Some((model, store.read(&pack_name, offset, length)?)))
}

/// 按描述子重排候选：返回 (按分数降序的命中, 缺少描述子的张数)。
///
/// 缺少记录或记录损坏都计入 missing；调用方决定是否整次拒绝，避免混入全局顺序。
pub fn rerank(
    database: &LibraryDb,
    store: &FeatureStore,
    kind: FeatureKind,
    model: &str,
    prepared: &PreparedDescriptor,
    image_ids: &[i64],
    limit: usize,
) -> AppResult<(Vec<(i64, f32)>, usize)> {
    let pointers = pointers(database, kind, model, image_ids)?;
    let mut by_pack: HashMap<String, Vec<(i64, u64)>> = HashMap::new();
    for (id, (pack_name, offset)) in pointers {
        by_pack.entry(pack_name).or_default().push((id, offset));
    }
    let record_len = kind
        .fixed_dim()
        .ok_or_else(|| AppError::message(format!("{} 不是语义描述子", kind.name())))?
        as usize;
    let mut scored = Vec::with_capacity(image_ids.len());
    for (pack_name, rows) in by_pack {
        let map = store.mmap(&pack_name)?;
        let partial: Vec<(i64, f32)> = rows
            .par_iter()
            .filter_map(|(id, offset)| {
                record_slice(&map, *offset, record_len)
                    .and_then(|record| prepared.score(record))
                    .map(|score| (*id, score))
            })
            .collect();
        scored.extend(partial);
    }
    let missing = image_ids.len().saturating_sub(scored.len());
    // 与 embedding 扫描同一套顺序：分数降序，同分按 id 升序。
    scored.sort_by(super::embedding::by_score_desc);
    scored.truncate(limit);
    Ok((scored, missing))
}

/// 批量读取候选图的描述子指针；缺失的 id 不会出现在结果里。
fn pointers(
    database: &LibraryDb,
    kind: FeatureKind,
    model: &str,
    image_ids: &[i64],
) -> AppResult<HashMap<i64, (String, u64)>> {
    let dim = kind
        .fixed_dim()
        .ok_or_else(|| AppError::message(format!("{} 不是语义描述子", kind.name())))?;
    let mut found = HashMap::with_capacity(image_ids.len());
    for chunk in image_ids.chunks(400) {
        let marks = placeholders(chunk.len());
        let sql = format!(
            "SELECT image_id, pack_name, pack_offset
             FROM feature_blobs
             WHERE kind = ?1 AND model = ?2 AND dim = ?3
               AND image_id IN ({marks})"
        );
        let mut values = vec![
            Value::Text(kind.name().to_owned()),
            Value::Text(model.to_owned()),
            Value::Integer(i64::from(dim)),
        ];
        values.extend(chunk.iter().map(|id| Value::Integer(*id)));
        let rows = database.with_reader(|connection| {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(params_from_iter(values.iter()), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as u64,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        for (id, pack_name, offset) in rows {
            found.insert(id, (pack_name, offset));
        }
    }
    Ok(found)
}

#[derive(Debug, Clone)]
pub struct QuantizedVector {
    pub dim: u32,
    pub values: Vec<i8>,
}

impl QuantizedVector {
    pub fn new(dim: u32, values: Vec<i8>) -> AppResult<Self> {
        if dim == 0 || values.len() != dim as usize {
            return Err(AppError::message(format!(
                "语义特征长度 {} 与声明维度 {dim} 不一致",
                values.len()
            )));
        }
        Ok(Self { dim, values })
    }
}

/// 追加一条 int8 记录并更新指针；`kind` 只接受 Spp / MaxSim。
pub fn store(
    database: &LibraryDb,
    store: &FeatureStore,
    image_id: i64,
    model: &str,
    kind: FeatureKind,
    vector: &QuantizedVector,
) -> AppResult<()> {
    let expected = kind
        .fixed_dim()
        .ok_or_else(|| AppError::message(format!("{} 不是语义描述子", kind.name())))?;
    if vector.dim != expected {
        return Err(AppError::message(format!(
            "{} 记录应为 {expected} 维，实际 {}",
            kind.name(),
            vector.dim
        )));
    }
    // 长度与内容的一致性由 QuantizedVector::new 与 FeatureStore::append 分别守住。
    let bytes: Vec<u8> = vector.values.iter().map(|value| *value as u8).collect();
    let location = store.append(kind, model, expected, FeatureDtype::I8, &bytes)?;
    write_pointer(database, image_id, kind, model, expected, &location)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::service;
    use rusqlite::params;

    fn fixture() -> (tempfile::TempDir, service::LibrarySession, FeatureStore) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "描述子测试", 1).unwrap();
        session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        id, content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (1, ?1, 'png', 1, 10, 4, 4, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params!["a".repeat(64)],
                )?;
                Ok(())
            })
            .unwrap();
        let store = FeatureStore::new();
        store.set_root(root.join(".artbank/features"));
        (temp, session, store)
    }

    #[test]
    fn stores_quantized_records_and_updates_pointers() {
        let (_temp, session, packs) = fixture();
        let spp = QuantizedVector::new(
            FeatureKind::SPP_RECORD_DIM,
            vec![3_i8; FeatureKind::SPP_RECORD_DIM as usize],
        )
        .unwrap();
        store(&session.db, &packs, 1, "model", FeatureKind::Spp, &spp).unwrap();
        let (kind, dim, length): (String, u32, u32) = session
            .db
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT kind, dim, pack_length FROM feature_blobs
                     WHERE image_id = 1 AND kind = 'spp'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .unwrap();
        assert_eq!(kind, "spp");
        assert_eq!(dim, FeatureKind::SPP_RECORD_DIM);
        assert_eq!(length, FeatureKind::SPP_RECORD_DIM);
    }

    fn descriptor_record(kind: FeatureKind, peaks: &[usize]) -> Vec<u8> {
        let block_dim = match kind {
            FeatureKind::Spp => FeatureKind::SPP_DIM_PER_CELL,
            FeatureKind::MaxSim => FeatureKind::MAXSIM_DIM_PER_LOCAL,
            _ => panic!("descriptor kind"),
        };
        let mut values = vec![0_u8; kind.fixed_dim().unwrap() as usize];
        for (block, peak) in peaks.iter().enumerate() {
            values[block * block_dim as usize + peak] = 127;
        }
        values
    }

    #[test]
    fn spp_scores_aligned_cells_above_shifted_ones() {
        let peaks: Vec<usize> = (0..64).collect();
        let prepared = PreparedDescriptor::new(
            FeatureKind::Spp,
            descriptor_record(FeatureKind::Spp, &peaks),
        )
        .unwrap();
        let same = descriptor_record(FeatureKind::Spp, &peaks);
        assert!((prepared.score(&same).unwrap() - 1.0).abs() < 1e-5);

        let shifted: Vec<usize> = (0..64).map(|index| (index + 1) % 64).collect();
        let shifted = descriptor_record(FeatureKind::Spp, &shifted);
        assert!(prepared.score(&shifted).unwrap().abs() < 1e-5);

        let mut near = same.clone();
        near[0] = 0;
        near[127] = 127;
        let near_score = prepared.score(&near).unwrap();
        assert!(near_score > 0.98 && near_score < 1.0, "{near_score}");
    }

    #[test]
    fn maxsim_is_unordered_and_prefers_matching_locals() {
        let peaks: Vec<usize> = (0..32).collect();
        let prepared = PreparedDescriptor::new(
            FeatureKind::MaxSim,
            descriptor_record(FeatureKind::MaxSim, &peaks),
        )
        .unwrap();
        let same = descriptor_record(FeatureKind::MaxSim, &peaks);
        assert!((prepared.score(&same).unwrap() - 1.0).abs() < 1e-5);

        // 局部顺序不同但集合相同：MaxSim 本来就不要求对齐。
        let mut permuted = peaks.clone();
        permuted.rotate_left(7);
        let permuted = descriptor_record(FeatureKind::MaxSim, &permuted);
        assert!((prepared.score(&permuted).unwrap() - 1.0).abs() < 1e-5);

        // 完全不同的局部集合：没有任何一对余弦为正。
        let shifted: Vec<usize> = (32..64).collect();
        let shifted = descriptor_record(FeatureKind::MaxSim, &shifted);
        assert!(prepared.score(&shifted).unwrap().abs() < 1e-5);
    }

    #[test]
    fn prepared_descriptor_checks_record_shape() {
        assert!(PreparedDescriptor::new(FeatureKind::Spp, vec![0; 10]).is_err());
        assert!(PreparedDescriptor::new(FeatureKind::MaxSim, vec![0; 10]).is_err());
        assert!(PreparedDescriptor::new(FeatureKind::Embedding, vec![0; 10]).is_err());
    }

    #[test]
    fn reads_target_and_candidate_pointers() {
        let (_temp, session, packs) = fixture();
        let spp = QuantizedVector::new(
            FeatureKind::SPP_RECORD_DIM,
            vec![3_i8; FeatureKind::SPP_RECORD_DIM as usize],
        )
        .unwrap();
        store(&session.db, &packs, 1, "model", FeatureKind::Spp, &spp).unwrap();

        let (model, bytes) = target(&session.db, &packs, 1, FeatureKind::Spp)
            .unwrap()
            .unwrap();
        assert_eq!(model, "model");
        assert_eq!(bytes.len(), FeatureKind::SPP_RECORD_DIM as usize);

        let pointers = pointers(&session.db, FeatureKind::Spp, "model", &[1, 2]).unwrap();
        assert_eq!(pointers.len(), 1);
        assert!(pointers.contains_key(&1));
    }

    #[test]
    fn reranks_candidates_and_reports_missing_records() {
        let (_temp, session, packs) = fixture();
        session
            .db
            .with_writer(|connection| {
                for id in 2..=3 {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', 1, 10, 4, 4, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        params![id, format!("{id:064x}")],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        let peaks: Vec<usize> = (0..64).collect();
        let query_bytes = descriptor_record(FeatureKind::Spp, &peaks);
        let shifted: Vec<usize> = (0..64).map(|index| (index + 1) % 64).collect();
        let shifted_bytes = descriptor_record(FeatureKind::Spp, &shifted);
        let as_vector = |bytes: &[u8]| QuantizedVector {
            dim: FeatureKind::SPP_RECORD_DIM,
            values: bytes.iter().map(|value| *value as i8).collect(),
        };
        store(
            &session.db,
            &packs,
            1,
            "model",
            FeatureKind::Spp,
            &as_vector(&query_bytes),
        )
        .unwrap();
        store(
            &session.db,
            &packs,
            2,
            "model",
            FeatureKind::Spp,
            &as_vector(&shifted_bytes),
        )
        .unwrap();
        store(
            &session.db,
            &packs,
            3,
            "model",
            FeatureKind::Spp,
            &as_vector(&shifted_bytes),
        )
        .unwrap();

        let prepared = PreparedDescriptor::new(FeatureKind::Spp, query_bytes).unwrap();
        let (hits, missing) = rerank(
            &session.db,
            &packs,
            FeatureKind::Spp,
            "model",
            &prepared,
            &[2, 3, 1],
            10,
        )
        .unwrap();
        assert_eq!(missing, 0);
        assert_eq!(hits[0].0, 1);
        assert!(hits[0].1 > hits[1].1);

        let (_, missing) = rerank(
            &session.db,
            &packs,
            FeatureKind::Spp,
            "model",
            &prepared,
            &[1, 99],
            10,
        )
        .unwrap();
        assert_eq!(missing, 1);
    }

    #[test]
    fn rejects_wrong_dimension_and_non_descriptor_kind() {
        let (_temp, session, packs) = fixture();
        assert!(QuantizedVector::new(4, vec![0; 3]).is_err());
        let wrong = QuantizedVector::new(4, vec![0; 4]).unwrap();
        assert!(store(&session.db, &packs, 1, "model", FeatureKind::Spp, &wrong).is_err());
        assert!(
            store(
                &session.db,
                &packs,
                1,
                "model",
                FeatureKind::Embedding,
                &wrong
            )
            .is_err()
        );
    }
}

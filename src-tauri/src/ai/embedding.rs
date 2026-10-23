use std::collections::BTreeMap;

use half::{f16, slice::HalfFloatSliceExt};
use rayon::prelude::*;
use rusqlite::params;

use crate::{
    app::error::{AppError, AppResult},
    library::db::LibraryDb,
    library::service::now_ms,
};

use super::pack::{FeatureDtype, FeatureKind, FeatureStore};

pub const DEFAULT_SEARCH_LIMIT: usize = 2_000;
pub const MAX_SEARCH_LIMIT: usize = 10_000;

#[derive(Debug, Clone)]
pub struct EmbeddingRow {
    pub image_id: i64,
    pub pack_name: String,
    pub offset: u64,
}

pub struct MeanEmbedding {
    pub vector: Vec<f32>,
    pub count: u64,
}

pub fn store(
    database: &LibraryDb,
    store: &FeatureStore,
    image_id: i64,
    model: &str,
    dim: u32,
    vector: &[f32],
) -> AppResult<()> {
    if vector.len() != dim as usize {
        return Err(AppError::message(format!(
            "embedding 维度 {} 与模型维度 {dim} 不一致",
            vector.len()
        )));
    }
    let normalized = normalize_f32(vector)?;
    let mut bytes = Vec::with_capacity(dim as usize * 2);
    for value in &normalized {
        bytes.extend_from_slice(&f16::from_f32(*value).to_bits().to_le_bytes());
    }
    let location = store.append(
        FeatureKind::Embedding,
        model,
        dim,
        FeatureDtype::F16,
        &bytes,
    )?;
    let now = now_ms();
    database.with_writer(|connection| {
        connection.execute(
            "INSERT INTO feature_blobs(
                image_id, kind, model, dim, pack_name, pack_offset, pack_length, updated_at
             ) VALUES (?1, 'embedding', ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(image_id, kind) DO UPDATE SET
                model = excluded.model,
                dim = excluded.dim,
                pack_name = excluded.pack_name,
                pack_offset = excluded.pack_offset,
                pack_length = excluded.pack_length,
                updated_at = excluded.updated_at",
            params![
                image_id,
                model,
                dim,
                location.pack_name,
                location.offset as i64,
                location.length as i64,
                now
            ],
        )?;
        Ok(())
    })
}

pub fn rows(database: &LibraryDb, model: &str, dim: u32) -> AppResult<Vec<EmbeddingRow>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT feature_blobs.image_id, feature_blobs.pack_name, feature_blobs.pack_offset
             FROM feature_blobs
             JOIN images ON images.id = feature_blobs.image_id
             WHERE feature_blobs.kind = 'embedding'
               AND feature_blobs.model = ?1
               AND feature_blobs.dim = ?2
               AND images.deleted_at IS NULL
             ORDER BY feature_blobs.pack_name, feature_blobs.pack_offset",
        )?;
        let rows = statement
            .query_map(params![model, dim], |row| {
                Ok(EmbeddingRow {
                    image_id: row.get(0)?,
                    pack_name: row.get(1)?,
                    offset: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

pub fn distinct_groups(database: &LibraryDb) -> AppResult<Vec<(String, u32, u64)>> {
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT feature_blobs.model, feature_blobs.dim, count(*)
             FROM feature_blobs
             JOIN images ON images.id = feature_blobs.image_id
             WHERE feature_blobs.kind = 'embedding' AND images.deleted_at IS NULL
             GROUP BY feature_blobs.model, feature_blobs.dim
             ORDER BY count(*) DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, i64>(2)?.max(0) as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

/// 用 fp16 记录还原出 fp32 均值；所有算术都在 f32 上完成。
pub fn mean(
    database: &LibraryDb,
    store: &FeatureStore,
    model: &str,
    dim: u32,
) -> AppResult<MeanEmbedding> {
    let rows = rows(database, model, dim)?;
    if rows.is_empty() {
        return Ok(MeanEmbedding {
            vector: vec![0.0; dim as usize],
            count: 0,
        });
    }
    let dim_usize = dim as usize;
    let mut sum = vec![0.0_f32; dim_usize];
    let mut buffer = vec![0.0_f32; dim_usize];
    let mut count = 0_u64;
    for (pack_name, pack_rows) in group_by_pack(rows) {
        let map = store.mmap(&pack_name)?;
        for row in pack_rows {
            let Some(bytes) = record_slice(&map, row.offset, dim_usize * 2) else {
                continue;
            };
            decode_f16(bytes, &mut buffer);
            let mut norm = 0.0_f32;
            let mut finite = true;
            for value in &buffer {
                if !value.is_finite() {
                    finite = false;
                    break;
                }
                norm += value * value;
            }
            if !finite || norm <= 1e-12 {
                continue;
            }
            for (slot, value) in sum.iter_mut().zip(buffer.iter()) {
                *slot += value;
            }
            count += 1;
        }
    }
    if count == 0 {
        return Ok(MeanEmbedding {
            vector: vec![0.0; dim_usize],
            count: 0,
        });
    }
    let divisor = count as f32;
    for value in &mut sum {
        *value /= divisor;
    }
    Ok(MeanEmbedding { vector: sum, count })
}

/// 中心化余弦：cosine(v - mean, q - mean)。计算全程 f32。
pub fn search(
    database: &LibraryDb,
    store: &FeatureStore,
    model: &str,
    dim: u32,
    query: &[f32],
    mean: &[f32],
    limit: usize,
) -> AppResult<Vec<(i64, f32)>> {
    let rows = rows(database, model, dim)?;
    search_rows(store, rows, dim, query, mean, limit)
}

/// 向量扫描只依赖行指针与 pack，不访问数据库；便于基准与后续复用。
pub fn search_rows(
    store: &FeatureStore,
    rows: Vec<EmbeddingRow>,
    dim: u32,
    query: &[f32],
    mean: &[f32],
    limit: usize,
) -> AppResult<Vec<(i64, f32)>> {
    if query.len() != dim as usize || mean.len() != dim as usize {
        return Err(AppError::message("查询向量与模型维度不一致"));
    }
    let query = normalize_f32(query)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let dim_usize = dim as usize;
    let mut dot_qq = 0.0_f32;
    let mut dot_mq = 0.0_f32;
    let mut dot_mm = 0.0_f32;
    for index in 0..dim_usize {
        let q = query[index];
        let m = mean[index];
        dot_qq += q * q;
        dot_mq += m * q;
        dot_mm += m * m;
    }
    let query_norm_sq = dot_qq - 2.0 * dot_mq + dot_mm;
    if query_norm_sq <= 1e-12 {
        return Err(AppError::message("查询向量与均值重合，无法排序"));
    }

    let mut scored = Vec::with_capacity(rows.len());
    for (pack_name, pack_rows) in group_by_pack(rows) {
        let map = store.mmap(&pack_name)?;
        let partial: Vec<(i64, f32)> = pack_rows
            .par_iter()
            .map_init(
                || vec![0.0_f32; dim_usize],
                |buffer, row| {
                    let bytes = record_slice(&map, row.offset, dim_usize * 2)?;
                    decode_f16(bytes, buffer);
                    let mut dot_vq = 0.0_f32;
                    let mut dot_vm = 0.0_f32;
                    let mut dot_vv = 0.0_f32;
                    for index in 0..dim_usize {
                        let value = buffer[index];
                        dot_vq += value * query[index];
                        dot_vm += value * mean[index];
                        dot_vv += value * value;
                    }
                    let vector_norm_sq = dot_vv - 2.0 * dot_vm + dot_mm;
                    if !vector_norm_sq.is_finite() || vector_norm_sq <= 1e-12 {
                        return None;
                    }
                    let centered_dot = dot_vq - dot_vm - dot_mq + dot_mm;
                    let score = centered_dot / (vector_norm_sq * query_norm_sq).sqrt();
                    score
                        .is_finite()
                        .then_some((row.image_id, score.clamp(-1.0, 1.0)))
                },
            )
            .flatten()
            .collect();
        scored.extend(partial);
    }

    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    let limit = limit.clamp(1, MAX_SEARCH_LIMIT);
    scored.truncate(limit);
    Ok(scored)
}

fn group_by_pack(rows: Vec<EmbeddingRow>) -> BTreeMap<String, Vec<EmbeddingRow>> {
    let mut grouped = BTreeMap::new();
    for row in rows {
        grouped
            .entry(row.pack_name.clone())
            .or_insert_with(Vec::new)
            .push(row);
    }
    grouped
}

fn record_slice(map: &[u8], offset: u64, length: usize) -> Option<&[u8]> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(length)?;
    map.get(start..end)
}

fn decode_f16(bytes: &[u8], output: &mut [f32]) {
    if bytes.len() == output.len() * 2 && (bytes.as_ptr() as usize).is_multiple_of(2) {
        // 包内记录按 2 字节对齐；批量转换比逐元素 from_bits 快数倍。
        let halves =
            unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<f16>(), output.len()) };
        halves.convert_to_f32_slice(output);
        return;
    }
    for (index, chunk) in bytes.chunks_exact(2).enumerate() {
        output[index] = f16::from_bits(u16::from_le_bytes([chunk[0], chunk[1]])).to_f32();
    }
}

fn normalize_f32(vector: &[f32]) -> AppResult<Vec<f32>> {
    let mut norm = 0.0_f32;
    for value in vector {
        if !value.is_finite() {
            return Err(AppError::message("embedding 包含非有限数值"));
        }
        norm += value * value;
    }
    let norm = norm.sqrt();
    if norm <= 1e-12 {
        return Err(AppError::message("embedding 是零向量"));
    }
    Ok(vector.iter().map(|value| value / norm).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::service;

    fn fixture() -> (
        tempfile::TempDir,
        service::LibrarySession,
        std::path::PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "AI 测试", 2).unwrap();
        std::fs::create_dir_all(root.join(".artbank/features")).unwrap();
        (temp, session, root)
    }

    fn insert_image(database: &LibraryDb, id: i64) {
        database
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO images(
                        id, content_hash, file_ext, folder_id, file_size, width, height, format,
                        has_alpha, phash, colors4, colors8, score, description, imported_at,
                        modified_at
                     ) VALUES (?1, ?2, 'png', 1, 10, 4, 4, 'png', 0,
                        zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                    params![id, format!("{id:064x}")],
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn mean_and_centered_search_are_exact() {
        let (_temp, session, root) = fixture();
        let packs = FeatureStore::new();
        packs.set_root(root.join(".artbank/features"));
        for id in 1..=3 {
            insert_image(&session.db, id);
        }
        store(&session.db, &packs, 1, "model", 4, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        store(&session.db, &packs, 2, "model", 4, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        store(&session.db, &packs, 3, "model", 4, &[0.0, 0.0, 1.0, 0.0]).unwrap();

        let mean = mean(&session.db, &packs, "model", 4).unwrap();
        assert_eq!(mean.count, 3);
        let results = search(
            &session.db,
            &packs,
            "model",
            4,
            &[0.9, 0.1, 0.0, 0.0],
            &mean.vector,
            3,
        )
        .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 1);
        assert!(results[0].1 >= results[1].1);
    }

    #[test]
    fn skips_missing_and_deleted_images() {
        let (_temp, session, root) = fixture();
        let packs = FeatureStore::new();
        packs.set_root(root.join(".artbank/features"));
        insert_image(&session.db, 1);
        store(&session.db, &packs, 1, "model", 4, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        session
            .db
            .with_writer(|connection| {
                connection.execute("UPDATE images SET deleted_at = 1 WHERE id = 1", [])?;
                Ok(())
            })
            .unwrap();
        assert!(rows(&session.db, "model", 4).unwrap().is_empty());
    }
}

#[cfg(test)]
mod perf_tests {
    use std::time::Instant;

    use super::*;

    /// 手动基准：20 万条 1536 维 fp16 向量扫描。
    /// cargo test --manifest-path src-tauri/Cargo.toml --lib ai::embedding::perf_tests -- --ignored --nocapture
    #[test]
    #[ignore = "20 万条向量基准，手动运行"]
    fn perf_embedding_scan_200k() {
        const COUNT: usize = 200_000;
        const DIM: u32 = 1536;
        let temp = tempfile::tempdir().unwrap();
        let packs = FeatureStore::new();
        packs.set_root(temp.path().to_path_buf());
        let mut rows = Vec::with_capacity(COUNT);
        let mut bytes = vec![0_u8; DIM as usize * 2];
        for index in 0..COUNT {
            for element in 0..DIM as usize {
                let value = (((index * 37 + element * 11) % 2000) as f32 / 1000.0) - 1.0;
                let encoded = half::f16::from_f32(value).to_bits().to_le_bytes();
                bytes[element * 2] = encoded[0];
                bytes[element * 2 + 1] = encoded[1];
            }
            let location = packs
                .append(
                    FeatureKind::Embedding,
                    "bench-model",
                    DIM,
                    FeatureDtype::F16,
                    &bytes,
                )
                .unwrap();
            rows.push(EmbeddingRow {
                image_id: index as i64 + 1,
                pack_name: location.pack_name,
                offset: location.offset,
            });
        }
        let mean = vec![0.001_f32; DIM as usize];
        let mut query = vec![0.0_f32; DIM as usize];
        query[0] = 1.0;
        let started = Instant::now();
        let results = search_rows(&packs, rows, DIM, &query, &mean, 2000).unwrap();
        let elapsed = started.elapsed();
        eprintln!(
            "embedding scan: {COUNT} rows × {DIM} dims in {:?}, top={:?}",
            elapsed,
            results.first()
        );
        assert_eq!(results.len(), 2000);
    }
}

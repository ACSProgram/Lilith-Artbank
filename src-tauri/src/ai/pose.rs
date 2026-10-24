use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        service::now_ms,
        similarity::MAX_LIMIT,
        viewer::{PoseData, PoseLandmark},
    },
};

use super::{
    pack::{FeatureDtype, FeatureKind, FeatureStore, record_slice},
    pose_similarity::{Pose as SimilarityPose, Weights as SimilarityWeights},
};

pub const LANDMARK_COUNT: usize = 33;
const VALUES_PER_LANDMARK: usize = 4;
const POSE_VALUES: usize = LANDMARK_COUNT * VALUES_PER_LANDMARK;
const POSE_BYTES: usize = POSE_VALUES * 4;

/// 写入 33 点 f32；不足 33 点按零填充（visibility=0 表示无效点，叠加层会跳过）。
pub fn store(
    database: &LibraryDb,
    store: &FeatureStore,
    image_id: i64,
    pose: &PoseData,
) -> AppResult<()> {
    let mut values = [0.0_f32; POSE_VALUES];
    for (index, landmark) in pose.landmarks.iter().take(LANDMARK_COUNT).enumerate() {
        let offset = index * VALUES_PER_LANDMARK;
        values[offset] = landmark.x;
        values[offset + 1] = landmark.y;
        values[offset + 2] = landmark.z;
        values[offset + 3] = landmark.visibility;
    }
    let mut bytes = Vec::with_capacity(POSE_BYTES);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let model = if pose.model.trim().is_empty() {
        "mediapipe-pose"
    } else {
        pose.model.trim()
    };
    // 包头的 dim 表示 f32 元素数（33 点 × 4 分量）；数据库仍记录 33 个 landmark。
    let location = store.append(
        FeatureKind::Pose,
        model,
        POSE_VALUES as u32,
        FeatureDtype::F32,
        &bytes,
    )?;
    let now = now_ms();
    database.with_writer(|connection| {
        connection.execute(
            "INSERT INTO feature_blobs(
                image_id, kind, model, dim, pack_name, pack_offset, pack_length, updated_at
             ) VALUES (?1, 'pose', ?2, ?3, ?4, ?5, ?6, ?7)
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
                LANDMARK_COUNT as i64,
                location.pack_name,
                location.offset as i64,
                location.length as i64,
                now
            ],
        )?;
        Ok(())
    })
}

/// 按 `feature_blobs` 指针单条读取；没有记录返回 `None`。
pub fn load(
    database: &LibraryDb,
    store: &FeatureStore,
    image_id: i64,
) -> AppResult<Option<PoseData>> {
    let pointer = database.with_reader(|connection| {
        let pointer = connection
            .query_row(
                "SELECT model, pack_name, pack_offset, pack_length
                 FROM feature_blobs WHERE image_id = ?1 AND kind = 'pose'",
                [image_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as u64,
                        row.get::<_, u32>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(pointer)
    })?;
    let Some((model, pack_name, offset, length)) = pointer else {
        return Ok(None);
    };
    if length as usize != POSE_BYTES {
        return Err(AppError::message("姿势记录长度无效"));
    }
    let bytes = store.read(&pack_name, offset, length)?;
    Ok(Some(PoseData {
        model,
        landmarks: decode_landmarks(&bytes),
    }))
}

/// 扫描同一模型的 pose 记录并按相似度降序返回；结果排除目标图自身。
///
/// 每个特征包只 mmap 一次再逐条切片，不为单条记录重复打开文件。
pub fn similar(
    database: &LibraryDb,
    store: &FeatureStore,
    target_id: i64,
    limit: usize,
) -> AppResult<Vec<(i64, f32)>> {
    let target = load(database, store, target_id)?
        .ok_or_else(|| AppError::message("这张图片还没有姿势数据，请先生成姿势"))?;
    let target_pose = SimilarityPose::from_landmarks(&target.landmarks);

    let mut by_pack: BTreeMap<String, Vec<(i64, u64)>> = BTreeMap::new();
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT feature_blobs.image_id, feature_blobs.pack_name,
                    feature_blobs.pack_offset, feature_blobs.pack_length
             FROM feature_blobs
             JOIN images ON images.id = feature_blobs.image_id
             WHERE feature_blobs.kind = 'pose'
               AND feature_blobs.model = ?1
               AND images.deleted_at IS NULL
             ORDER BY feature_blobs.pack_name, feature_blobs.pack_offset",
        )?;
        let mut rows = statement.query_map([&target.model], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, u32>(3)?,
            ))
        })?;
        for row in rows.by_ref() {
            let (image_id, pack_name, offset, length) = row?;
            if image_id == target_id || length as usize != POSE_BYTES {
                continue;
            }
            by_pack
                .entry(pack_name)
                .or_default()
                .push((image_id, offset));
        }
        Ok(())
    })?;

    let weights = SimilarityWeights::default();
    let mut scored = Vec::new();
    for (pack_name, records) in by_pack {
        let map = match store.mmap(&pack_name) {
            Ok(map) => map,
            Err(error) => {
                log::warn!("无法映射 pose 包 {pack_name}：{error}");
                continue;
            }
        };
        for (image_id, offset) in records {
            let Some(bytes) = record_slice(&map, offset, POSE_BYTES) else {
                continue;
            };
            let pose = SimilarityPose::from_landmarks(&decode_landmarks(bytes));
            scored.push((image_id, target_pose.similarity(&pose, &weights) as f32));
        }
    }
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.truncate(limit.clamp(1, MAX_LIMIT));
    Ok(scored)
}

fn decode_landmarks(bytes: &[u8]) -> Vec<PoseLandmark> {
    let mut landmarks = Vec::with_capacity(LANDMARK_COUNT);
    for chunk in bytes.chunks_exact(VALUES_PER_LANDMARK * 4) {
        let read = |index: usize| {
            f32::from_le_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ])
        };
        landmarks.push(PoseLandmark {
            x: read(0),
            y: read(1),
            z: read(2),
            visibility: read(3),
        });
    }
    landmarks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::service;

    #[test]
    fn round_trips_pose_and_pads_missing_points() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "姿势测试", 1).unwrap();
        std::fs::create_dir_all(root.join(".artbank/features")).unwrap();
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

        let packs = FeatureStore::new();
        packs.set_root(root.join(".artbank/features"));
        let pose = PoseData {
            model: "mediapipe-pose".into(),
            landmarks: vec![PoseLandmark {
                x: 0.25,
                y: 0.5,
                z: -0.1,
                visibility: 0.9,
            }],
        };
        store(&session.db, &packs, 1, &pose).unwrap();
        let loaded = load(&session.db, &packs, 1).unwrap().unwrap();
        assert_eq!(loaded.landmarks.len(), LANDMARK_COUNT);
        assert!((loaded.landmarks[0].x - 0.25).abs() < 1e-6);
        assert!((loaded.landmarks[0].visibility - 0.9).abs() < 1e-6);
        assert_eq!(loaded.landmarks[32].visibility, 0.0);
        assert!(load(&session.db, &packs, 2).unwrap().is_none());
    }

    #[test]
    fn similar_orders_by_score_and_excludes_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "姿势相似测试", 1).unwrap();
        std::fs::create_dir_all(root.join(".artbank/features")).unwrap();
        for id in 1..=3 {
            session
                .db
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
        let packs = FeatureStore::new();
        packs.set_root(root.join(".artbank/features"));

        let mut base = vec![
            PoseLandmark {
                x: 0.5,
                y: 0.5,
                z: 0.0,
                visibility: 1.0,
            };
            LANDMARK_COUNT
        ];
        for (index, x, y) in [
            (11, 0.42, 0.3),
            (12, 0.58, 0.3),
            (13, 0.36, 0.45),
            (14, 0.64, 0.45),
            (15, 0.33, 0.6),
            (16, 0.67, 0.6),
            (23, 0.45, 0.55),
            (24, 0.55, 0.55),
            (25, 0.44, 0.72),
            (26, 0.56, 0.72),
            (27, 0.43, 0.9),
            (28, 0.57, 0.9),
        ] {
            base[index] = PoseLandmark {
                x,
                y,
                z: 0.0,
                visibility: 1.0,
            };
        }
        let mut different = base.clone();
        different[13] = PoseLandmark {
            x: 0.36,
            y: 0.2,
            z: 0.0,
            visibility: 1.0,
        };
        different[15] = PoseLandmark {
            x: 0.33,
            y: 0.1,
            z: 0.0,
            visibility: 1.0,
        };

        for (id, landmarks) in [
            (1_i64, base.clone()),
            (2_i64, base.clone()),
            (3_i64, different),
        ] {
            store(
                &session.db,
                &packs,
                id,
                &PoseData {
                    model: "mediapipe-pose".into(),
                    landmarks,
                },
            )
            .unwrap();
        }

        let results = similar(&session.db, &packs, 1, 10).unwrap();
        assert_eq!(results[0].0, 2);
        assert!((results[0].1 - 1.0).abs() < 1e-6);
        assert_eq!(results[1].0, 3);
        assert!(results[1].1 < results[0].1);
        assert!(similar(&session.db, &packs, 99, 10).is_err());
    }
}

use rusqlite::{OptionalExtension, params};

use crate::{
    app::error::{AppError, AppResult},
    library::{
        db::LibraryDb,
        service::now_ms,
        viewer::{PoseData, PoseLandmark},
    },
};

use super::pack::{FeatureDtype, FeatureKind, FeatureStore};

pub const LANDMARK_COUNT: usize = 33;
const VALUES_PER_LANDMARK: usize = 4;
const POSE_VALUES: usize = LANDMARK_COUNT * VALUES_PER_LANDMARK;
const POSE_BYTES: usize = POSE_VALUES * 4;

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
    Ok(Some(PoseData { model, landmarks }))
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
}

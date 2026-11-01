//! SPP / MaxSim 描述子的 int8 落盘。
//!
//! 两条记录都由服务端在同一次图片塔前向里算好并量化；这里只校验维度、追加到
//! feature pack 并更新 `feature_blobs` 指针。量化 scale 固定在白化基里，不进记录。

use crate::{
    app::error::{AppError, AppResult},
    library::db::LibraryDb,
};

use super::pack::{FeatureDtype, FeatureKind, FeatureStore, write_pointer};

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

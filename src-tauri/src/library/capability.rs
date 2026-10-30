//! 图片能力查询：只在前端打开右键菜单/详情时按需调用，
//! 不进入列表查询，避免给每页 150 行加多个 EXISTS。

use rusqlite::params_from_iter;

use crate::{
    app::error::AppResult,
    library::{db::LibraryDb, ids, placeholders},
};

const MAX_IDS: usize = 500;
const CHUNK: usize = 200;

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageCapabilities {
    pub image_id: i64,
    pub pose: bool,
    pub embedding: bool,
    pub colors: bool,
    pub phash: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureSummary {
    pub description: u64,
    pub embedding: u64,
    pub pose: u64,
    pub phash: u64,
    pub colors: u64,
}

pub fn image_capabilities(
    database: &LibraryDb,
    image_ids: &[i64],
) -> AppResult<Vec<ImageCapabilities>> {
    let unique = ids::unique_positive(image_ids, "图片")?;
    ids::ensure_max(
        unique.len(),
        MAX_IDS,
        format!("一次最多查询 {MAX_IDS} 张图片的能力"),
    )?;

    database.with_reader(|connection| {
        let mut result = Vec::with_capacity(unique.len());
        for chunk in unique.chunks(CHUNK) {
            let marks = placeholders(chunk.len());
            let sql = format!(
                "SELECT images.id,
                        EXISTS(SELECT 1 FROM feature_blobs
                               WHERE feature_blobs.image_id = images.id
                                 AND feature_blobs.kind = 'pose'),
                        EXISTS(SELECT 1 FROM feature_blobs
                               WHERE feature_blobs.image_id = images.id
                                 AND feature_blobs.kind = 'embedding'),
                        length(images.colors4) = 16 AND length(images.colors8) = 32,
                        length(images.phash) = 32
                 FROM images WHERE images.id IN ({marks})"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
                Ok(ImageCapabilities {
                    image_id: row.get(0)?,
                    pose: row.get::<_, i64>(1)? != 0,
                    embedding: row.get::<_, i64>(2)? != 0,
                    colors: row.get::<_, i64>(3)? != 0,
                    phash: row.get::<_, i64>(4)? != 0,
                })
            })?;
            result.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(result)
    })
}

pub fn feature_summary(database: &LibraryDb) -> AppResult<FeatureSummary> {
    // 临时文件夹是会话级暂存区，不计入图库的特征覆盖度。
    let exclude = crate::library::temp::EXCLUDE_SQL;
    let sql = format!(
        "SELECT
            (SELECT count(*) FROM images
             WHERE deleted_at IS NULL AND description <> '' AND {exclude}),
            (SELECT count(DISTINCT feature_blobs.image_id)
             FROM feature_blobs JOIN images ON images.id = feature_blobs.image_id
             WHERE feature_blobs.kind = 'embedding' AND images.deleted_at IS NULL
               AND {exclude}),
            (SELECT count(DISTINCT feature_blobs.image_id)
             FROM feature_blobs JOIN images ON images.id = feature_blobs.image_id
             WHERE feature_blobs.kind = 'pose' AND images.deleted_at IS NULL
               AND {exclude}),
            (SELECT count(*) FROM images
             WHERE deleted_at IS NULL AND length(phash) = 32 AND {exclude}),
            (SELECT count(*) FROM images
             WHERE deleted_at IS NULL
               AND length(colors4) = 16 AND length(colors8) = 32 AND {exclude})"
    );
    database.with_reader(|connection| {
        let summary = connection.query_row(&sql, [], |row| {
            Ok(FeatureSummary {
                description: row.get::<_, i64>(0)?.max(0) as u64,
                embedding: row.get::<_, i64>(1)?.max(0) as u64,
                pose: row.get::<_, i64>(2)?.max(0) as u64,
                phash: row.get::<_, i64>(3)?.max(0) as u64,
                colors: row.get::<_, i64>(4)?.max(0) as u64,
            })
        })?;
        Ok(summary)
    })
}

/// 临时区自己的 embedding 与姿势数量；与 [`FeatureSummary`] 分开，
/// 避免 AI 把临时特征误当成整库覆盖度。
pub fn temp_feature_summary(database: &LibraryDb) -> AppResult<(u64, u64)> {
    let only = crate::library::temp::ONLY_SQL;
    database.with_reader(|connection| {
        Ok(connection.query_row(
            &format!(
                "SELECT
                    (SELECT count(DISTINCT feature_blobs.image_id)
                     FROM feature_blobs JOIN images ON images.id = feature_blobs.image_id
                     WHERE feature_blobs.kind = 'embedding' AND images.deleted_at IS NULL
                       AND {only}),
                    (SELECT count(DISTINCT feature_blobs.image_id)
                     FROM feature_blobs JOIN images ON images.id = feature_blobs.image_id
                     WHERE feature_blobs.kind = 'pose' AND images.deleted_at IS NULL
                       AND {only})"
            ),
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?.max(0) as u64,
                    row.get::<_, i64>(1)?.max(0) as u64,
                ))
            },
        )?)
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::library::service;

    #[test]
    fn reports_capabilities_and_summary() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "能力测试", 1).unwrap();
        session
            .db
            .with_writer(|connection| {
                for (id, description) in [(1_i64, "描述"), (2, "")] {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', 1, 1, 1, 1, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, ?3, 1, 1)",
                        params![id, format!("{id:0>64}"), description],
                    )?;
                }
                connection.execute(
                    "INSERT INTO feature_blobs(image_id, kind, model, dim, pack_name,
                        pack_offset, pack_length, updated_at)
                     VALUES (1, 'pose', 'm', 33, 'p.bin', 0, 12, 1)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let caps = image_capabilities(&session.db, &[1, 2, 1]).unwrap();
        assert_eq!(caps.len(), 2);
        assert!(caps[0].pose && caps[0].phash && caps[0].colors && !caps[0].embedding);
        assert!(!caps[1].pose && !caps[1].embedding);

        let summary = feature_summary(&session.db).unwrap();
        assert_eq!(summary.description, 1);
        assert_eq!(summary.pose, 1);
        assert_eq!(summary.embedding, 0);
        assert_eq!(summary.phash, 2);
    }
}

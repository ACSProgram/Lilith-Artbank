use std::sync::Arc;

use rusqlite::params;

use super::*;
use crate::ai::{
    pack::FeatureStore,
    test_support::{MockResponse, MockServer},
};
use crate::library::service;

fn image_png() -> Vec<u8> {
    let image = image::RgbImage::from_fn(32, 24, |x, y| {
        image::Rgb([(x * 5) as u8, (y * 7) as u8, 90])
    });
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn setup() -> (
    tempfile::TempDir,
    service::LibrarySession,
    std::path::PathBuf,
    Arc<FeatureStore>,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("library");
    let (session, _) = service::create(&root, "AI 任务测试", 2).unwrap();
    let bytes = image_png();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    std::fs::write(root.join(format!("{hash}.png")), &bytes).unwrap();
    session
        .db
        .with_writer(|connection| {
            connection.execute(
                "INSERT INTO images(
                    id, content_hash, file_ext, folder_id, file_size, width, height, format,
                    has_alpha, phash, colors4, colors8, score, description, imported_at,
                    modified_at
                 ) VALUES (1, ?1, 'png', 1, ?2, 32, 24, 'png', 0,
                    zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                params![hash, bytes.len() as i64],
            )?;
            Ok(())
        })
        .unwrap();
    let store = Arc::new(FeatureStore::new());
    store.set_root(root.join(".artbank/features"));
    (temp, session, root, store)
}

fn context(
    session: &service::LibrarySession,
    root: &std::path::Path,
    store: Arc<FeatureStore>,
    base_url: String,
) -> JobContext {
    JobContext {
        database: session.db.clone(),
        thumbs: session.thumbs.clone(),
        root: root.to_path_buf(),
        store,
        settings: AiSettings {
            base_url,
            ..AiSettings::default()
        },
        resources: Arc::new(crate::concurrency::resources::ResourceTable::new()),
        images: Arc::new(crate::concurrency::budget::ImageBudget::new(2)),
    }
}

fn mock_server() -> MockServer {
    MockServer::start(|request, _| {
        if request.path.starts_with("/v1/health") {
            MockResponse::json(
                200,
                serde_json::json!({
                    "status": "ok",
                    "version": "1",
                    "capabilities": {
                        "imageEmbedding": {"ready": true, "model": "siglip2-test", "dim": 4},
                        "textEmbedding": {"ready": true, "model": "siglip2-test", "dim": 4},
                        "pose": {"ready": true, "model": "mediapipe-pose"},
                        "description": {"ready": true, "model": "desc-test"}
                    }
                })
                .to_string(),
            )
        } else if request.path.starts_with("/v1/embed/image") {
            MockResponse::json(
                200,
                serde_json::json!({
                    "status": "success",
                    "model": "siglip2-test",
                    "dim": 4,
                    "normalized": true,
                    "vector": [0.6, 0.8, 0.0, 0.0]
                })
                .to_string(),
            )
        } else if request.path.starts_with("/v1/pose") {
            let landmarks: Vec<_> = (0..33)
                .map(|index| {
                    serde_json::json!({
                        "x": index as f64 / 33.0,
                        "y": 0.5,
                        "z": 0.0,
                        "visibility": 0.9
                    })
                })
                .collect();
            MockResponse::json(
                200,
                serde_json::json!({
                    "status": "success",
                    "model": "mediapipe-pose",
                    "landmarks": landmarks
                })
                .to_string(),
            )
        } else if request.path.starts_with("/v1/describe") {
            MockResponse::json(
                200,
                serde_json::json!({
                    "status": "success",
                    "model": "desc-test",
                    "text": "一名人物站在画面中央。",
                    "truncated": false
                })
                .to_string(),
            )
        } else {
            MockResponse::json(404, "{}")
        }
    })
}

fn request(kind: AiJobKind) -> AiJobRequest {
    AiJobRequest {
        kind,
        image_ids: vec![1],
        overwrite: false,
        prompt: None,
        max_tokens: None,
    }
}

fn task(name: &str, kind: AiJobKind, scope: &str) -> std::sync::Arc<TaskHandle> {
    TaskHandle::new(name, kind.task_kind(), scope)
}

#[test]
fn runs_embedding_pose_and_description_jobs() {
    let (_temp, session, root, store) = setup();
    let server = mock_server();
    let context = context(&session, &root, store, server.url());

    let job = task("embed", AiJobKind::Embedding, "选中 1 张");
    execute(&context, &job, &request(AiJobKind::Embedding))
        .unwrap_or_else(|error| panic!("{error}; requests={:?}", server.requests()));
    assert_eq!(job.snapshot().completed, 1);
    let embedding = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT model, dim FROM feature_blobs WHERE image_id = 1 AND kind = 'embedding'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(embedding, ("siglip2-test".into(), 4));

    let job = task("pose", AiJobKind::Pose, "选中 1 张");
    execute(&context, &job, &request(AiJobKind::Pose))
        .unwrap_or_else(|error| panic!("{error}; requests={:?}", server.requests()));
    assert_eq!(job.snapshot().completed, 1);
    let pose_dim: u32 = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT dim FROM feature_blobs WHERE image_id = 1 AND kind = 'pose'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(pose_dim, 33);

    let job = task("desc", AiJobKind::Description, "选中 1 张");
    execute(&context, &job, &request(AiJobKind::Description))
        .unwrap_or_else(|error| panic!("{error}; requests={:?}", server.requests()));
    assert_eq!(job.snapshot().completed, 1);
    let description = session
        .db
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT description, description_source, description_model
                 FROM images WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?)
        })
        .unwrap();
    assert_eq!(description.0, "一名人物站在画面中央。");
    assert_eq!(description.1, "ai");
    assert_eq!(description.2, "desc-test");
}

#[test]
fn never_overwrites_handwritten_description() {
    let (_temp, session, root, store) = setup();
    session
        .db
        .with_writer(|connection| {
            connection.execute(
                "UPDATE images SET description = '手写描述', description_source = 'user'
                 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let server = mock_server();
    let context = context(&session, &root, store, server.url());
    let job = task("desc", AiJobKind::Description, "选中 1 张");
    let mut value = request(AiJobKind::Description);
    value.overwrite = true;
    execute(&context, &job, &value).unwrap();
    assert_eq!(job.snapshot().total, 0);
    let description: String = session
        .db
        .with_reader(|connection| {
            Ok(
                connection.query_row("SELECT description FROM images WHERE id = 1", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(description, "手写描述");
}

#[test]
fn rejects_long_prompt_and_bad_tokens() {
    let request = AiJobRequest {
        prompt: Some("a".repeat(2_001)),
        ..AiJobRequest::default()
    };
    assert!(request.normalized().is_err());
    let request = AiJobRequest {
        max_tokens: Some(8),
        ..AiJobRequest::default()
    };
    assert!(request.normalized().is_err());
}

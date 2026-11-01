//! 语义特征白化基的归属、身份与准备。
//!
//! 基是图库派生数据：`<library>/.artbank/basis/<clip_model>/` 下的两份 npz。
//! 模型目录里的同名文件只是通用兜底。基身份由两份文件字节与模型名算出，
//! 同时写进 `meta.feature_basis` 与 `spp` / `maxsim` 的包名。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{
    app::{
        error::{AppError, AppResult},
        paths,
        settings::AiSettings,
        tasks::{TaskHandle, TaskPhase},
    },
    concurrency::{
        budget::{ImageBudget, ImageMemoryBudget},
        resources::{HolderInfo, ResourceTable},
    },
    library::{db::LibraryDb, paths::LibraryPaths, service::now_ms, thumbs::ThumbPack},
};

use super::{
    client::{AiClient, BasisRequest, BasisResult},
    input,
    pack::{FeatureDtype, FeatureKind, pack_name},
};

pub const BASIS_DIR: &str = "basis";
pub const SPP_FILE: &str = "spp_basis.npz";
pub const MAXSIM_FILE: &str = "maxsim_basis.npz";
/// 库内基缺失时，少于这么多有效图片就复制通用兜底，不自行拟合。
pub const MIN_FIT_IMAGES: u64 = 1_500;
pub const FIT_QUOTA: usize = 3_000;
pub const FIT_PER_FOLDER: usize = 60;
pub const FIT_MAXSIM_SAMPLE: u32 = 1_400;
pub const META_KEY: &str = "feature_basis";
/// 旧基的包名在切基后先记在这里，确认没有任何行引用后再删文件。
pub const RELEASE_KEY: &str = "feature.pendingRelease.v1";

pub const SOURCE_LIBRARY: &str = "library";
pub const SOURCE_UNIVERSAL: &str = "universal";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureBasis {
    pub model: String,
    pub id: String,
    pub samples: u32,
    /// `library` 表示库内拟合或手工放入，`universal` 表示复制自模型目录。
    pub source: String,
    pub fitted_at: i64,
}

#[derive(Debug, Clone)]
pub struct BasisPaths {
    pub model: String,
    pub dir: PathBuf,
    pub spp: PathBuf,
    pub maxsim: PathBuf,
}

impl BasisPaths {
    fn from_dir(model: String, dir: PathBuf) -> Self {
        Self {
            spp: dir.join(SPP_FILE),
            maxsim: dir.join(MAXSIM_FILE),
            model,
            dir,
        }
    }

    pub fn request(&self, id: &str) -> BasisRequest {
        BasisRequest {
            dir: self.dir.to_string_lossy().into_owned(),
            id: id.to_owned(),
        }
    }
}

/// 当前库内可用的基：路径与身份。
#[derive(Debug, Clone)]
pub struct ActiveBasis {
    pub paths: BasisPaths,
    pub id: String,
}

/// 模型目录名；与服务端 `ClipModelInfo.model_name` 同一口径。
pub fn model_name(settings: &AiSettings) -> Option<String> {
    let dir = paths::clip_model_dir(
        settings.model_path.as_deref(),
        settings.clip_model_path.as_deref(),
    )?;
    dir.file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// 通用兜底基所在的模型目录。
pub fn universal_paths(settings: &AiSettings) -> Option<BasisPaths> {
    let dir = paths::clip_model_dir(
        settings.model_path.as_deref(),
        settings.clip_model_path.as_deref(),
    )?;
    let model = model_name(settings)?;
    Some(BasisPaths::from_dir(model, dir))
}

/// 库内基目录；模型名来自模型目录的最后一段。
pub fn library_paths(settings: &AiSettings, root: &Path) -> Option<BasisPaths> {
    let model = model_name(settings)?;
    let dir = LibraryPaths::new(root).data.join(BASIS_DIR).join(&model);
    Some(BasisPaths::from_dir(model, dir))
}

/// `blake3(模型名 | 两份 npz 字节)` 的前 16 位。
pub fn compute_id(model: &str, spp: &[u8], maxsim: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(model.as_bytes());
    hasher.update(b"|spp|");
    hasher.update(spp);
    hasher.update(b"|maxsim|");
    hasher.update(maxsim);
    let digest = hasher.finalize().to_hex();
    digest[..16].to_owned()
}

pub fn load_meta(database: &LibraryDb) -> AppResult<Option<FeatureBasis>> {
    let raw: Option<String> = database.with_reader(|connection| {
        Ok(connection
            .query_row("SELECT value FROM meta WHERE key = ?1", [META_KEY], |row| {
                row.get(0)
            })
            .optional()?)
    })?;
    Ok(raw.and_then(|raw| serde_json::from_str::<FeatureBasis>(&raw).ok()))
}

pub fn save_meta(database: &LibraryDb, basis: &FeatureBasis) -> AppResult<()> {
    let value = serde_json::to_string(basis)?;
    database.with_writer(|connection| {
        connection.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![META_KEY, value],
        )?;
        Ok(())
    })
}

/// 读取库内基；两份文件必须都在。半套文件按“没有基”处理，不用一份正确一份错误去加载塔。
pub fn active_basis(settings: &AiSettings, root: &Path) -> AppResult<Option<ActiveBasis>> {
    let Some(paths) = library_paths(settings, root) else {
        return Ok(None);
    };
    let Some(spp) = read_if_exists(&paths.spp)? else {
        return Ok(None);
    };
    let Some(maxsim) = read_if_exists(&paths.maxsim)? else {
        return Ok(None);
    };
    let id = compute_id(&paths.model, &spp, &maxsim);
    Ok(Some(ActiveBasis { paths, id }))
}

/// 图库打开时的轻量准备：库内已有基就直接用；只有小库复制通用兜底。
///
/// 大库不在这里复制通用兜底，也不拟合：等到第一次生成语义特征时再按自己的分布
/// 拟合专用基，开库不做重活。库内文件已经换了、但 meta 还指向旧基时**不改 meta**：
/// 描述子还没重算完，旧包要继续服务；等批量任务成功后再由 [`commit`] 切换。
pub fn prepare_local(
    database: &LibraryDb,
    settings: &AiSettings,
    root: &Path,
) -> AppResult<Option<FeatureBasis>> {
    let meta = load_meta(database)?;
    if let Some(active) = active_basis(settings, root)? {
        if let Some(meta) = meta {
            return Ok(Some(meta));
        }
        let basis = FeatureBasis {
            model: active.paths.model.clone(),
            id: active.id.clone(),
            samples: 0,
            source: SOURCE_LIBRARY.into(),
            fitted_at: now_ms(),
        };
        save_meta(database, &basis)?;
        return Ok(Some(basis));
    }
    let count = active_image_count(database)?;
    if count >= MIN_FIT_IMAGES {
        log::info!("图库有 {count} 张有效图片，首次生成语义特征时再拟合专用基");
        return Ok(meta);
    }
    // 库内文件缺失：即使 meta 还指向旧基，也要重新准备物理基，否则新描述子无从生成。
    // meta 先保留旧身份，等批量任务补齐后由 `commit` 切换；旧包继续服务未处理的图片。
    let Some(universal) = universal_paths(settings) else {
        return Ok(None);
    };
    let Some(spp) = read_if_exists(&universal.spp)? else {
        return Ok(None);
    };
    let Some(maxsim) = read_if_exists(&universal.maxsim)? else {
        return Ok(None);
    };
    let Some(local) = library_paths(settings, root) else {
        return Ok(None);
    };
    let id = compute_id(&local.model, &spp, &maxsim);
    write_pair(&local, &spp, &maxsim)?;
    if let Some(meta) = meta {
        log::info!(
            "库内基缺失，已复制通用兜底基；旧 meta 保持到批量任务补齐：{}",
            local.dir.display()
        );
        return Ok(Some(meta));
    }
    let basis = FeatureBasis {
        model: local.model,
        id,
        samples: 0,
        source: SOURCE_UNIVERSAL.into(),
        fitted_at: now_ms(),
    };
    save_meta(database, &basis)?;
    log::info!("已复制通用语义特征基到库内：{}", local.dir.display());
    Ok(Some(basis))
}

/// 任务跑完后把 meta 指向新基，并登记旧基的包名等待释放。
///
/// 新基与 meta 相同只补样本数；失败或取消的任务不会调用这里（由调用方判断），
/// 旧基继续服务未处理的图片。
pub fn commit(
    database: &LibraryDb,
    root: &Path,
    active: &ActiveBasis,
    samples: Option<u32>,
) -> AppResult<FeatureBasis> {
    let previous = load_meta(database)?;
    if previous.as_ref().is_some_and(|meta| meta.id == active.id) {
        let mut meta = previous.expect("checked above");
        if let Some(samples) = samples
            && samples > 0
            && meta.samples != samples
        {
            meta.samples = samples;
            save_meta(database, &meta)?;
        }
        return Ok(meta);
    }
    if let Some(previous) = &previous {
        mark_pack_release(database, previous)?;
    }
    let basis = FeatureBasis {
        model: active.paths.model.clone(),
        id: active.id.clone(),
        samples: samples.filter(|samples| *samples > 0).unwrap_or(0),
        source: SOURCE_LIBRARY.into(),
        fitted_at: now_ms(),
    };
    save_meta(database, &basis)?;
    let removed = release_pending(database, root)?;
    if removed > 0 {
        log::info!("切基后已释放 {removed} 个旧特征包");
    }
    Ok(basis)
}

/// 记录旧基对应的 spp / maxsim 包名。
fn mark_pack_release(database: &LibraryDb, old: &FeatureBasis) -> AppResult<()> {
    let mut names = load_release(database)?;
    for kind in [FeatureKind::Spp, FeatureKind::MaxSim] {
        let dim = kind.fixed_dim().expect("描述子有固定维度");
        names.push(pack_name(
            kind,
            &old.model,
            dim,
            FeatureDtype::I8,
            Some(&old.id),
        ));
    }
    names.sort();
    names.dedup();
    save_release(database, &names)
}

fn load_release(database: &LibraryDb) -> AppResult<Vec<String>> {
    let raw: Option<String> = database.with_reader(|connection| {
        Ok(connection
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [RELEASE_KEY],
                |row| row.get(0),
            )
            .optional()?)
    })?;
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default())
}

fn save_release(database: &LibraryDb, names: &[String]) -> AppResult<()> {
    let value = serde_json::to_string(names)?;
    database.with_writer(|connection| {
        connection.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![RELEASE_KEY, value],
        )?;
        Ok(())
    })
}

/// 删除不再被任何行引用的待释放特征包；文件被读句柄占住时保留到下次。
pub fn release_pending(database: &LibraryDb, root: &Path) -> AppResult<u64> {
    let names = load_release(database)?;
    if names.is_empty() {
        return Ok(0);
    }
    let features_dir = LibraryPaths::new(root).features;
    let mut kept = Vec::new();
    let mut removed = 0_u64;
    for name in names {
        let referenced: bool = database.with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM feature_blobs WHERE pack_name = ?1)",
                [&name],
                |row| row.get::<_, i64>(0),
            )? != 0)
        })?;
        if referenced {
            kept.push(name);
            continue;
        }
        match std::fs::remove_file(features_dir.join(&name)) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => removed += 1,
            Err(error) => {
                log::warn!("删除旧特征包 {name} 失败：{error}");
                kept.push(name);
            }
        }
    }
    save_release(database, &kept)?;
    Ok(removed)
}

/// 分层抽样：单目录上限 + 按顺序等距取满配额。
pub fn sample_ids(database: &LibraryDb, quota: usize, per_folder: usize) -> AppResult<Vec<i64>> {
    if quota == 0 {
        return Ok(Vec::new());
    }
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "WITH ranked AS (
                 SELECT images.id AS id,
                        ROW_NUMBER() OVER (
                            PARTITION BY images.folder_id ORDER BY images.id
                        ) AS rn
                 FROM images
                 JOIN folders ON folders.id = images.folder_id
                 WHERE images.deleted_at IS NULL AND folders.kind <> 'temp'
             ),
             kept AS (
                 SELECT id,
                        ROW_NUMBER() OVER (ORDER BY id) AS n,
                        count(*) OVER () AS total
                 FROM ranked
                 WHERE rn <= ?1
             )
             SELECT id FROM kept
             WHERE (n - 1) % max(1, (total + ?2 - 1) / ?2) = 0
             ORDER BY n
             LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![per_folder as i64, quota as i64], |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

pub fn active_image_count(database: &LibraryDb) -> AppResult<u64> {
    database.with_reader(|connection| {
        Ok(connection.query_row(
            "SELECT count(*) FROM images
             JOIN folders ON folders.id = images.folder_id
             WHERE images.deleted_at IS NULL AND folders.kind <> 'temp'",
            [],
            |row| row.get::<_, i64>(0).map(|count| count.max(0) as u64),
        )?)
    })
}

/// 自动拟合所需的图库句柄与资源。
pub struct FitContext {
    pub database: Arc<LibraryDb>,
    pub thumbs: Arc<ThumbPack>,
    pub root: PathBuf,
    pub settings: AiSettings,
    pub resources: Arc<ResourceTable>,
    pub images: Arc<ImageBudget>,
    pub memory: Arc<ImageMemoryBudget>,
}

/// 分层抽样 → 逐张上传 → 拟合 → 落盘，返回库内基。
///
/// 取消返回 `None`。这里不写 `meta`：由调用方在描述子批量任务成功后用 [`commit`]
/// 切换，失败或取消时旧基与旧包继续服务。
pub fn fit(context: &FitContext, task: &Arc<TaskHandle>) -> AppResult<Option<ActiveBasis>> {
    let Some(paths) = library_paths(&context.settings, &context.root) else {
        return Err(AppError::message(
            "没有配置 CLIP 模型目录，无法拟合语义特征基",
        ));
    };
    task.set_phase(TaskPhase::Preparing);
    task.set_message("正在抽样…");
    let ids = sample_ids(&context.database, FIT_QUOTA, FIT_PER_FOLDER)?;
    if ids.len() < 2 {
        return Err(AppError::message("有效图片太少，无法拟合语义特征基"));
    }
    task.set_total(ids.len());
    task.set_phase(TaskPhase::Running);
    task.set_message("正在生成样本特征…");

    let cancel_task = task.clone();
    let client = AiClient::cancellable(
        &context.settings.base_url,
        Arc::new(move || cancel_task.should_cancel()),
    );
    let sample_id = client
        .basis_start(&paths.model, FIT_MAXSIM_SAMPLE)
        .map_err(|error| AppError::message(error.to_string()))?;
    let holder = HolderInfo::new(task.id(), "准备语义特征基").with_path_note("正在读取");
    let cancel = || task.should_cancel();

    let outcome = (|| -> AppResult<Option<BasisResult>> {
        for (index, image_id) in ids.iter().enumerate() {
            if cancel() {
                return Ok(None);
            }
            task.set_current(Some(*image_id));
            let bytes = input::small_bytes(
                &context.database,
                &context.thumbs,
                &context.root,
                *image_id,
                &context.resources,
                &context.images,
                &context.memory,
                &holder,
                &cancel,
            )?;
            client
                .basis_add(&sample_id, &bytes)
                .map_err(|error| AppError::message(error.to_string()))?;
            task.set_completed(index + 1);
        }
        task.set_message("正在拟合白化基…");
        let result = client
            .basis_finish(&sample_id)
            .map_err(|error| AppError::message(error.to_string()))?;
        Ok(Some(result))
    })();

    let (spp, maxsim, samples) = match outcome {
        Ok(Some(result)) => (result.spp, result.maxsim, result.samples),
        Ok(None) => {
            let _ = client.basis_cancel(&sample_id);
            return Ok(None);
        }
        Err(error) => {
            let _ = client.basis_cancel(&sample_id);
            return Err(error);
        }
    };

    let id = compute_id(&paths.model, &spp, &maxsim);
    write_pair(&paths, &spp, &maxsim)?;
    task.set_message(format!("语义特征基已就绪（{samples} 张样本）"));
    Ok(Some(ActiveBasis { paths, id }))
}

fn read_if_exists(path: &Path) -> AppResult<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// 写入两份基。先写临时文件再改名，避免换基换到一半留下半份文件。
fn write_pair(paths: &BasisPaths, spp: &[u8], maxsim: &[u8]) -> AppResult<()> {
    std::fs::create_dir_all(&paths.dir)?;
    for (path, bytes) in [(&paths.spp, spp), (&paths.maxsim, maxsim)] {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| AppError::message("基文件名无效"))?;
        let temp = paths.dir.join(format!("{file_name}.tmp"));
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::service;

    fn session() -> (tempfile::TempDir, service::LibrarySession, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "基测试", 1).unwrap();
        (temp, session, root)
    }

    #[test]
    fn basis_id_is_stable_and_identity_sensitive() {
        let first = compute_id("model", b"spp", b"maxsim");
        assert_eq!(first.len(), 16);
        assert_eq!(first, compute_id("model", b"spp", b"maxsim"));
        assert_ne!(first, compute_id("other", b"spp", b"maxsim"));
        assert_ne!(first, compute_id("model", b"spp", b"other"));
    }

    #[test]
    fn saves_and_loads_meta() {
        let (_temp, session, _root) = session();
        assert!(load_meta(&session.db).unwrap().is_none());
        let basis = FeatureBasis {
            model: "model".into(),
            id: "0123456789abcdef".into(),
            samples: 42,
            source: SOURCE_LIBRARY.into(),
            fitted_at: 123,
        };
        save_meta(&session.db, &basis).unwrap();
        assert_eq!(load_meta(&session.db).unwrap().unwrap(), basis);
    }

    #[test]
    fn samples_hit_quota_and_folder_cap() {
        let (_temp, session, _root) = session();
        session
            .db
            .with_writer(|connection| {
                for folder in [1, 3] {
                    connection.execute(
                        "INSERT Or IGNORE INTO folders(id, path, created_at) VALUES (?1, ?2, 1)",
                        params![folder, format!("f{folder}")],
                    )?;
                }
                for id in 1..=40 {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', ?3, 10, 4, 4, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        params![id, format!("{id:064x}"), if id <= 30 { 1 } else { 3 }],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        let sampled = sample_ids(&session.db, 10, 5).unwrap();
        assert_eq!(sampled.len(), 10);
        let from_first = sampled.iter().filter(|id| **id <= 30).count();
        assert!(from_first <= 5, "单目录上限失效：{sampled:?}");
        assert_eq!(active_image_count(&session.db).unwrap(), 40);
    }

    #[test]
    fn changing_basis_makes_every_descriptor_pending() {
        use crate::ai::{
            descriptors, embedding,
            pack::{FeatureKind, FeatureStore},
        };
        use crate::library::query::{self, LibraryQuery, PendingFilter};

        let (_temp, session, root) = session();
        session
            .db
            .with_writer(|connection| {
                for id in 1..=2 {
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
        let store = FeatureStore::new();
        store.set_root(LibraryPaths::new(&root).features);
        let old_basis = "0123456789abcdef";
        let new_basis = "fedcba9876543210";
        for id in 1..=2 {
            embedding::store(&session.db, &store, id, "model", 4, &[1.0, 0.0, 0.0, 0.0]).unwrap();
            for kind in [FeatureKind::Spp, FeatureKind::MaxSim] {
                let vector = descriptors::QuantizedVector {
                    dim: kind.fixed_dim().unwrap(),
                    values: vec![1_i8; kind.fixed_dim().unwrap() as usize],
                };
                descriptors::store(
                    &session.db,
                    &store,
                    id,
                    "model",
                    Some(old_basis),
                    kind,
                    &vector,
                )
                .unwrap();
            }
        }
        store.release_handles();
        save_meta(
            &session.db,
            &FeatureBasis {
                model: "model".into(),
                id: old_basis.into(),
                samples: 2,
                source: SOURCE_LIBRARY.into(),
                fitted_at: 1,
            },
        )
        .unwrap();

        let pending = |basis_id: &str| {
            let filter = PendingFilter::Features {
                model: "model".into(),
                dims: crate::ai::pack::semantic_feature_demands("model", 4, Some(basis_id)),
            };
            query::pending_stats(&session.db, LibraryQuery::default(), &filter)
                .unwrap()
                .will_process
        };
        assert_eq!(pending(old_basis), 0);
        assert_eq!(pending(new_basis), 2);
    }

    #[test]
    fn copies_universal_basis_when_library_has_none() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "兜底基测试", 1).unwrap();
        let model_dir = temp.path().join("clip-model");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join(SPP_FILE), b"universal-spp").unwrap();
        std::fs::write(model_dir.join(MAXSIM_FILE), b"universal-maxsim").unwrap();
        let settings = AiSettings {
            model_path: None,
            clip_model_path: Some(model_dir.to_string_lossy().into_owned()),
            ..AiSettings::default()
        };

        let basis = prepare_local(&session.db, &settings, &root)
            .unwrap()
            .expect("应复制通用基");
        assert_eq!(basis.source, SOURCE_UNIVERSAL);
        assert!(basis.id.len() == 16);
        let local = library_paths(&settings, &root).unwrap();
        assert_eq!(std::fs::read(&local.spp).unwrap(), b"universal-spp");
        assert_eq!(load_meta(&session.db).unwrap().unwrap().id, basis.id);
        // 库内已有基后不再覆盖 meta。
        let again = prepare_local(&session.db, &settings, &root)
            .unwrap()
            .unwrap();
        assert_eq!(again.id, basis.id);

        // 手工删掉库内基：自动重新复制，但 meta 保持旧身份直到批量任务补齐。
        std::fs::remove_file(&local.spp).unwrap();
        std::fs::remove_file(&local.maxsim).unwrap();
        let restored = prepare_local(&session.db, &settings, &root)
            .unwrap()
            .unwrap();
        assert_eq!(restored.id, basis.id);
        assert!(local.spp.is_file() && local.maxsim.is_file());
    }

    #[test]
    fn large_library_waits_for_fit_instead_of_copying_universal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "大库基测试", 1).unwrap();
        let model_dir = temp.path().join("clip-model");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(model_dir.join(SPP_FILE), b"universal-spp").unwrap();
        std::fs::write(model_dir.join(MAXSIM_FILE), b"universal-maxsim").unwrap();
        let settings = AiSettings {
            model_path: None,
            clip_model_path: Some(model_dir.to_string_lossy().into_owned()),
            ..AiSettings::default()
        };
        session
            .db
            .with_writer(|connection| {
                for id in 1..=MIN_FIT_IMAGES as i64 {
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

        assert!(
            prepare_local(&session.db, &settings, &root)
                .unwrap()
                .is_none()
        );
        let local = library_paths(&settings, &root).unwrap();
        assert!(!local.spp.exists() && !local.maxsim.exists());
    }
}

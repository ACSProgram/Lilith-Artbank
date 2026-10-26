//! 临时区特征生成。
//!
//! 临时文件夹是“先看再决定”的入口；AI 可以把候选图导入临时区后，用这个工具补
//! embedding 或姿势特征，再通过 `find_similar(scope=temp)` 判断库里有没有相近的图。
//!
//! 边界：只接受临时图片 id，混入普通图片或已删除 id 时整批拒绝；单次最多 100 张；
//! 不生成描述、不写操作记录；任务占用 write lane，询问档每次都要确认。

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ai::job::AiJobKind,
    app::{
        error::{AppError, AppResult},
        settings::McpMode,
        state::AppState,
        tasks::TaskStartResult,
    },
    library::{db::LibraryDb, placeholders},
};

use super::{ToolOutcome, write};

pub const MAX_TEMP_FEATURE_IDS: usize = 100;

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TempFeaturesInput {
    #[schemars(description = "临时文件夹里的图片 id，最多 100 张；混入普通图片时整批拒绝")]
    pub ids: Vec<i64>,
    #[schemars(description = "embedding 或 pose")]
    pub kind: String,
}

pub fn temp_features_generate(
    state: &AppState,
    input: &TempFeaturesInput,
) -> AppResult<ToolOutcome> {
    run(state, input, false)
}

pub fn execute_temp_features(
    state: &AppState,
    input: &TempFeaturesInput,
) -> AppResult<ToolOutcome> {
    run(state, input, true)
}

fn run(state: &AppState, input: &TempFeaturesInput, approved: bool) -> AppResult<ToolOutcome> {
    let mode = write::ensure_writable(state)?;
    let kind = parse_kind(&input.kind)?;
    let ids = normalize_ids(&input.ids)?;
    let context = state.mcp_context()?;
    let invalid = invalid_ids(&context.database, &ids)?;
    if !invalid.is_empty() {
        return Ok(ToolOutcome::value(json!({
            "status": "rejected",
            "reason": "ids 里混入了不存在、已删除或不在临时文件夹的图片；整批没有执行",
            "invalidIds": invalid,
        })));
    }

    let preview = preview(kind, ids.len());
    if mode == McpMode::Ask && !approved {
        return write::enqueue_approval(
            state,
            "temp_features_generate",
            format!("AI · 为临时区 {} 张图片生成{}", ids.len(), kind_label(kind)),
            input,
            preview,
        );
    }

    let started = state.start_temp_features(kind, ids.clone())?;
    let task_id = match started {
        TaskStartResult::Started { task } => task.task_id,
        TaskStartResult::Conflict { active } => {
            let label = active
                .first()
                .map(|task| task.label.as_str())
                .unwrap_or("其他任务");
            return Err(AppError::message(format!(
                "「{label}」正在进行，临时区特征生成已拒绝"
            )));
        }
    };
    state.remember_mcp_task(&task_id);
    Ok(ToolOutcome::task(
        task_id.clone(),
        json!({
            "status": "started",
            "taskId": task_id,
            "kind": kind_label(kind),
            "count": ids.len(),
            "preview": preview,
            "message": "特征只写入临时区，不写操作记录；关闭、切换图库或退出应用会清空。用 tasks_get 轮询。",
        }),
    ))
}

fn parse_kind(value: &str) -> AppResult<AiJobKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "embedding" => Ok(AiJobKind::Embedding),
        "pose" => Ok(AiJobKind::Pose),
        other => Err(AppError::message(format!(
            "kind 只能是 embedding 或 pose，收到 {other}"
        ))),
    }
}

fn kind_label(kind: AiJobKind) -> &'static str {
    match kind {
        AiJobKind::Embedding => "embedding",
        AiJobKind::Pose => "姿势",
        AiJobKind::Description => "描述",
    }
}

fn normalize_ids(ids: &[i64]) -> AppResult<Vec<i64>> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if *id <= 0 {
            return Err(AppError::message("图片 id 无效"));
        }
        if seen.insert(*id) {
            result.push(*id);
        }
    }
    if result.is_empty() {
        return Err(AppError::message("请至少提供一张临时区图片"));
    }
    if result.len() > MAX_TEMP_FEATURE_IDS {
        return Err(AppError::message(format!(
            "一次最多为 {MAX_TEMP_FEATURE_IDS} 张临时图片生成特征"
        )));
    }
    Ok(result)
}

/// 返回不满足条件的 id 与原因；顺序与输入一致。
fn invalid_ids(database: &LibraryDb, ids: &[i64]) -> AppResult<Vec<Value>> {
    let found = database.with_reader(|connection| {
        let marks = placeholders(ids.len());
        let sql = format!(
            "SELECT images.id, images.deleted_at IS NOT NULL,
                    COALESCE(folders.kind = 'temp', 0)
             FROM images
             LEFT JOIN folders ON folders.id = images.folder_id
             WHERE images.id IN ({marks})"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, i64>(2)? != 0,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let states: std::collections::HashMap<i64, (bool, bool)> = found
        .into_iter()
        .map(|(id, deleted, temporary)| (id, (deleted, temporary)))
        .collect();
    Ok(ids
        .iter()
        .filter_map(|id| {
            let reason = match states.get(id) {
                None => "不存在",
                Some((true, _)) => "已在回收站",
                Some((false, false)) => "不在临时文件夹",
                Some((false, true)) => return None,
            };
            Some(json!({ "id": id, "reason": reason }))
        })
        .collect())
}

fn preview(kind: AiJobKind, count: usize) -> Value {
    json!({
        "operation": "生成临时区特征",
        "target": "临时文件夹",
        "count": count,
        "unit": "image",
        "featureKind": kind_label(kind),
        "consequences": [
            "只写入 embedding 或姿势特征，不生成描述",
            "不写操作记录；临时区关闭、切换图库或退出应用时清空",
            "清空后特征包里的字节不会立刻回收，留待维护任务处理"
        ],
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::library::service;

    fn fixture() -> (tempfile::TempDir, service::LibrarySession) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("library");
        let (session, _) = service::create(&root, "临时特征测试", 1).unwrap();
        let temp_folder = session
            .db
            .with_writer(|connection| crate::library::temp::require(connection))
            .unwrap();
        session
            .db
            .with_writer(|connection| {
                connection.execute(
                    "INSERT INTO folders(path, created_at) VALUES ('作品', 1)",
                    [],
                )?;
                let works = connection.last_insert_rowid();
                for (id, folder) in [(1_i64, temp_folder.id), (2, works)] {
                    connection.execute(
                        "INSERT INTO images(
                            id, content_hash, file_ext, folder_id, file_size, width, height,
                            format, has_alpha, phash, colors4, colors8, score, description,
                            imported_at, modified_at
                         ) VALUES (?1, ?2, 'png', ?3, 1, 1, 1, 'png', 0,
                            zeroblob(32), zeroblob(16), zeroblob(32), 0, '', 1, 1)",
                        params![id, format!("{id:0>64}"), folder],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        (temp, session)
    }

    #[test]
    fn rejects_mixed_and_missing_ids_without_partial_work() {
        let (_temp, session) = fixture();
        let invalid = invalid_ids(&session.db, &[1, 2, 3]).unwrap();
        assert_eq!(invalid.len(), 2);
        assert_eq!(invalid[0]["id"], 2);
        assert_eq!(invalid[0]["reason"], "不在临时文件夹");
        assert_eq!(invalid[1]["id"], 3);
        assert_eq!(invalid[1]["reason"], "不存在");
    }

    #[test]
    fn accepts_only_temp_ids_within_limit() {
        let (_temp, session) = fixture();
        assert!(invalid_ids(&session.db, &[1]).unwrap().is_empty());
        assert!(normalize_ids(&[1, 1, 2]).unwrap() == vec![1, 2]);
        assert!(normalize_ids(&[]).is_err());
        assert!(normalize_ids(&(1..=101).collect::<Vec<_>>()).is_err());
    }

    #[test]
    fn parses_embedding_and_pose_only() {
        assert!(matches!(
            parse_kind("embedding").unwrap(),
            AiJobKind::Embedding
        ));
        assert!(matches!(parse_kind("pose").unwrap(), AiJobKind::Pose));
        assert!(parse_kind("description").is_err());
    }
}

pub mod history;
pub mod read;
pub mod task;
pub mod write;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::app::{
    error::{AppError, AppResult},
    state::AppState,
};

use super::approval::ApprovalAction;

/// 工具执行结果；异步工具额外带任务 id，批准流程用它把确认单和任务绑定。
pub struct ToolOutcome {
    pub value: Value,
    pub task_id: Option<String>,
}

impl ToolOutcome {
    pub fn value(value: Value) -> Self {
        Self {
            value,
            task_id: None,
        }
    }

    pub fn task(task_id: String, value: Value) -> Self {
        Self {
            value,
            task_id: Some(task_id),
        }
    }
}

/// 用户批准后按工具名重新调用同一个领域函数；写入函数此时跳过询问判定。
pub fn execute_approval(state: &AppState, action: &ApprovalAction) -> AppResult<ToolOutcome> {
    match action.tool.as_str() {
        "images_update" => write::execute_update(state, &parse(&action.arguments)?),
        "images_move" => write::execute_move(state, &parse(&action.arguments)?),
        "images_trash" => write::execute_trash(state, &parse(&action.arguments)?),
        "images_restore" => write::execute_restore(state, &parse(&action.arguments)?),
        "collections_create" => write::execute_collection_create(state, &parse(&action.arguments)?),
        // 重命名/删除集合不受询问档拦截，批准路径与直接调用是同一个入口。
        "collections_modify" => write::collections_modify(state, &parse(&action.arguments)?),
        "collections_membership" => {
            write::execute_collection_membership(state, &parse(&action.arguments)?)
        }
        "history_undo" => history::execute_undo(state, &parse(&action.arguments)?),
        "history_redo" => history::execute_redo(state, &parse(&action.arguments)?),
        other => Err(AppError::message(format!("未知的待确认工具：{other}"))),
    }
}

fn parse<T: DeserializeOwned>(value: &Value) -> AppResult<T> {
    serde_json::from_value(value.clone())
        .map_err(|error| AppError::message(format!("待确认参数无法解析：{error}")))
}

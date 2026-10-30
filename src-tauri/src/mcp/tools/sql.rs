//! `library_sql`：只读 SQL 分析与导出入口。

use rmcp::schemars;
use serde::Deserialize;

use crate::{
    app::{
        error::{AppError, AppResult},
        state::AppState,
    },
    library::{paths::LibraryPaths, sql_query},
    mcp::{audit, tools::ToolOutcome},
};

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySqlInput {
    /// 单条 SELECT 或以 WITH 开头的查询；只能访问 mcp_* 视图。
    pub sql: String,
    /// 把完整结果写成文件而不是内联返回；默认 false。
    #[serde(default)]
    pub export: bool,
    /// 导出格式 jsonl（默认）或 csv；export=false 时忽略。
    pub format: Option<String>,
}

pub fn library_sql(state: &AppState, input: &LibrarySqlInput) -> AppResult<ToolOutcome> {
    if input.sql.trim().is_empty() {
        return Err(AppError::message("SQL 不能为空"));
    }
    let context = state.mcp_context()?;
    let database = LibraryPaths::new(context.root.clone()).database;
    let kind = if input.export {
        sql_query::QueryKind::Export(sql_query::ExportFormat::parse(input.format.as_deref())?)
    } else {
        sql_query::QueryKind::Inline
    };
    let outcome = state.sql_sandbox().run(&database, &input.sql, kind)?;
    audit::sql_query(
        &input.sql,
        outcome.row_count,
        outcome.bytes,
        outcome
            .export
            .as_ref()
            .map(|export| export.path.to_string_lossy().into_owned())
            .as_deref(),
    );
    Ok(ToolOutcome::value(outcome.to_json()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_format_parsing_defaults_to_jsonl() {
        assert!(sql_query::ExportFormat::parse(Some("sqlite")).is_err());
        assert_eq!(
            sql_query::ExportFormat::parse(None).unwrap(),
            sql_query::ExportFormat::Jsonl
        );
        assert_eq!(
            sql_query::ExportFormat::parse(Some("CSV")).unwrap(),
            sql_query::ExportFormat::Csv
        );
    }
}

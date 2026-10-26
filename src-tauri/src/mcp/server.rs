use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use tauri::{AppHandle, Manager};

use crate::{
    ai::runtime::AiRuntime,
    app::{error::AppResult, state::AppState},
};

use super::{
    context::ReadContext,
    tools::{
        self, ToolOutcome,
        history::{HistoryListInput, HistoryRunInput},
        read::{
            self, FindSimilarInput, FoldersListInput, ImageSheetInput, ImagesGetInput,
            SearchImagesInput, TagsListInput,
        },
        task::{self, TasksCancelInput, TasksGetInput},
        write::{
            self, CollectionsCreateInput, CollectionsMembershipInput, CollectionsModifyInput,
            ImagesMoveInput, ImagesTrashInput, ImagesUpdateInput,
        },
    },
};

/// 已注册的工具数，由 `library_info` 报给客户端；测试保证它与路由一致。
const TOOL_COUNT: usize = 20;

/// 工具适配器只负责取当前 AppState、调用领域函数并把结果编码成 MCP 内容。
#[derive(Clone)]
pub struct McpServer {
    app: AppHandle,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
        }
    }

    /// 在阻塞线程里跑一次工具逻辑：取当前图库快照，再交给领域函数。
    async fn with_context<T, F>(&self, operation: F) -> Result<AppResult<T>, ErrorData>
    where
        T: Send + 'static,
        F: FnOnce(&ReadContext, &AiRuntime) -> AppResult<T> + Send + 'static,
    {
        let app = self.app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let state = app.state::<AppState>();
            let context = state.mcp_context()?;
            operation(&context, &state.ai)
        })
        .await
        .map_err(|error| ErrorData::internal_error(format!("MCP 工具执行失败：{error}"), None))
    }

    /// 写工具与任务工具需要完整的 `AppState`（任务总线、确认队列、图库句柄）。
    async fn with_state<F>(&self, operation: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(&AppState) -> AppResult<ToolOutcome> + Send + 'static,
    {
        let app = self.app.clone();
        let result = tauri::async_runtime::spawn_blocking(move || {
            let state = app.state::<AppState>();
            operation(&state)
        })
        .await
        .map_err(|error| ErrorData::internal_error(format!("MCP 工具执行失败：{error}"), None))?;
        Ok(outcome_result(result))
    }
}

fn outcome_result(result: AppResult<ToolOutcome>) -> CallToolResult {
    match result {
        Ok(outcome) => CallToolResult::success(vec![ContentBlock::text(outcome.value.to_string())]),
        Err(error) => tool_error(error),
    }
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "图库概况：名称、路径、图片/回收站计数、schema 版本、特征数量、AI 配置与 MCP 限制。只读。"
    )]
    async fn library_info(&self) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context(|context, _| read::library_info(context, TOOL_COUNT))
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "列文件夹及其直接/子树图片数。parentPath 省略时列根目录；给相对路径时列它的直接子文件夹。"
    )]
    async fn folders_list(
        &self,
        Parameters(input): Parameters<FoldersListInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context(move |context, _| read::folders_list(context, &input))
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "列标签及图片数；query 按名称模糊匹配，limit 默认 100。没有图片的标签不会返回。"
    )]
    async fn tags_list(
        &self,
        Parameters(input): Parameters<TagsListInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context(move |context, _| read::tags_list(context, &input))
            .await?;
        Ok(tool_value(result))
    }

    #[tool(description = "列集合及图片数，按用户排序。")]
    async fn collections_list(&self) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context(|context, _| read::collections_list(context))
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "结构化搜索图片，返回总数、紧凑行与下一页游标；pageSize 范围 1-50，超出直接报错。search 会匹配描述与标签名，view=library 时包含回收站。includeFacets 会额外计算分面计数，较慢，按需开启。"
    )]
    async fn search_images(
        &self,
        Parameters(input): Parameters<SearchImagesInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context(move |context, _| read::search_images(context, &input))
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "找相似：mode=image 用目标图与 kind（phash/color4/color8/embedding/pose）；mode=text 用语义搜索。total 是全部命中数，returned 是可翻页/全选的条数（受 limit 截断）。scoreKind=distance 时 score 越小越近，similarity 时越大越近。结果只有一个结果槽，下一次 find_similar 会让当前 token 失效。"
    )]
    async fn find_similar(
        &self,
        Parameters(input): Parameters<FindSimilarInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context(move |context, ai| read::find_similar(context, ai, &input))
            .await?;
        Ok(tool_value(result))
    }

    #[tool(description = "按 id 批量取详情、标签、描述与能力标记；一次最多 50 张。")]
    async fn images_get(
        &self,
        Parameters(input): Parameters<ImagesGetInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context(move |context, _| read::images_get(context, &input.ids))
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "把 selector 的前 N 张图拼成 1-5 格拼图，返回图片、A1… 格子映射、sheetId 与下一页。格子标签始终按新拼图布局编号；以拼图格子为来源时用 sourceLabel 记录来源。thumb 读缩略图；detail 直接解码原图且最多 3×3。"
    )]
    async fn image_sheet(
        &self,
        Parameters(input): Parameters<ImageSheetInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context(move |context, ai| read::image_sheet(context, ai, &input))
            .await?;
        match result {
            Ok(sheet) => {
                let mapping = serde_json::to_string(&sheet.mapping).unwrap_or_else(|error| {
                    format!(r#"{{"error":"拼图映射序列化失败：{error}"}}"#)
                });
                let mut content = vec![ContentBlock::text(mapping)];
                content.push(ContentBlock::image(
                    STANDARD.encode(sheet.image),
                    "image/jpeg",
                ));
                Ok(CallToolResult::success(content))
            }
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        description = "批量修改评分、加标签、删标签或单图描述；一次只能做一类。先 dryRun 看 selected，正式写入必须带 confirmedCount且数量一致；一次调用只写一条操作记录。询问档超过 100 张需要用户确认，只读档拒绝。"
    )]
    async fn images_update(
        &self,
        Parameters(input): Parameters<ImagesUpdateInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| write::images_update(state, &input))
            .await
    }

    #[tool(
        description = "把 selector 解析的图片移动到目标文件夹（后台任务）。返回 taskId，完成后任务快照带 operationId。正式写入需 confirmedCount；询问档超过 100 张需确认。"
    )]
    async fn images_move(
        &self,
        Parameters(input): Parameters<ImagesMoveInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| write::images_move(state, &input))
            .await
    }

    #[tool(
        description = "把 selector 解析的图片移入回收站；不会删除原图。正式写入需 confirmedCount；询问档不论张数都要用户确认。"
    )]
    async fn images_trash(
        &self,
        Parameters(input): Parameters<ImagesTrashInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| write::images_trash(state, &input))
            .await
    }

    #[tool(
        description = "恢复 selector 解析的回收站图片。正式写入需 confirmedCount；询问档超过 100 张需确认。"
    )]
    async fn images_restore(
        &self,
        Parameters(input): Parameters<ImagesTrashInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| write::images_restore(state, &input))
            .await
    }

    #[tool(
        description = "创建集合，可选把 selector 解析的图片一起加入（同一次调用只产生一条记录）。带 selector 时正式写入需 confirmedCount；超过 100 张在询问档需确认。"
    )]
    async fn collections_create(
        &self,
        Parameters(input): Parameters<CollectionsCreateInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| write::collections_create(state, &input))
            .await
    }

    #[tool(
        description = "重命名或删除集合。删除可带 expectedItems 核对集合当前图片数；不一致直接拒绝。"
    )]
    async fn collections_modify(
        &self,
        Parameters(input): Parameters<CollectionsModifyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| write::collections_modify(state, &input))
            .await
    }

    #[tool(
        description = "把 selector 解析的图片加入或移出集合。正式写入需 confirmedCount；询问档超过 100 张需确认。"
    )]
    async fn collections_membership(
        &self,
        Parameters(input): Parameters<CollectionsMembershipInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| write::collections_membership(state, &input))
            .await
    }

    #[tool(
        description = "列操作记录，包含状态、影响数、从这条开始撤销/重做会动几条、是否可执行与阻塞原因；kind 可过滤。"
    )]
    async fn history_list(
        &self,
        Parameters(input): Parameters<HistoryListInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| tools::history::history_list(state, &input))
            .await
    }

    #[tool(
        description = "撤销到指定记录：撤销该记录及之后所有 applied 记录。先 dryRun 看 steps/affected，正式执行必须带一致的 confirmedSteps 与 confirmedAffected；询问档总需确认；撤销链含压缩记录会被明确拒绝，不能绕过。"
    )]
    async fn history_undo(
        &self,
        Parameters(input): Parameters<HistoryRunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| tools::history::history_undo(state, &input))
            .await
    }

    #[tool(
        description = "重做当前状态到指定记录之间的连续 undone 记录。先 dryRun 看 steps/affected，正式执行必须带一致的 confirmedSteps 与 confirmedAffected；询问档总需确认。新写入会清空可重做分支。"
    )]
    async fn history_redo(
        &self,
        Parameters(input): Parameters<HistoryRunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| tools::history::history_redo(state, &input))
            .await
    }

    #[tool(
        description = "查询任务或待确认单。不带 id 时返回活动任务、最近结束的任务与待确认列表；waitMs 最多 30000。用 approvalId 查询会等待用户批准/拒绝并附上关联任务。"
    )]
    async fn tasks_get(
        &self,
        Parameters(input): Parameters<TasksGetInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| task::tasks_get(state, &input))
            .await
    }

    #[tool(
        description = "取消 MCP 自己发起的任务；传 approvalId 等同于拒绝这次待确认操作。用户从界面启动的任务不能取消。"
    )]
    async fn tasks_cancel(
        &self,
        Parameters(input): Parameters<TasksCancelInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state(move |state| task::tasks_cancel(state, &input))
            .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "lilith-artbank",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Lilith Artbank 本地图库。可浏览、搜索、查看详情与拼图，也可批量改评分/标签/描述、移动、移入回收站、维护集合，以及查看与撤销/重做操作记录。写入受应用内权限档限制：只读档拒绝全部写入，询问档会把危险或超量操作放入待确认队列；没有永久删除、清空回收站、压缩与导入工具。",
            )
    }
}

fn tool_value(result: crate::app::error::AppResult<serde_json::Value>) -> CallToolResult {
    match result {
        Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
        Err(error) => tool_error(error),
    }
}

fn tool_error(error: impl ToString) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_count_matches_registered_tools() {
        assert_eq!(McpServer::tool_router().list_all().len(), TOOL_COUNT);
    }

    #[test]
    fn write_tools_are_registered() {
        let names: Vec<String> = McpServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        for expected in [
            "images_update",
            "images_move",
            "images_trash",
            "images_restore",
            "collections_create",
            "collections_modify",
            "collections_membership",
            "history_list",
            "history_undo",
            "history_redo",
            "tasks_get",
            "tasks_cancel",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "缺少工具 {expected}"
            );
        }
    }
}

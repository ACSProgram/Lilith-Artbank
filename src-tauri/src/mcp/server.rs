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
    tools::read::{
        self, FindSimilarInput, FoldersListInput, ImageSheetInput, ImagesGetInput,
        SearchImagesInput, TagsListInput,
    },
};

/// 已注册的工具数，由 `library_info` 报给客户端；测试保证它与路由一致。
const TOOL_COUNT: usize = 8;

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

    #[tool(description = "列标签及图片数；query 按名称模糊匹配，limit 默认 100。")]
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
        description = "结构化搜索图片，返回总数、紧凑行与下一页游标；pageSize 最多 50。includeFacets 会额外计算分面计数，较慢，按需开启。"
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
        description = "找相似：mode=image 用目标图与 kind（phash/color4/color8/embedding/pose）；mode=text 用语义搜索。结果只有一个结果槽，下一次 find_similar 会让当前 token 失效。"
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
        description = "把 selector 的前 N 张图拼成 1-5 格拼图，返回图片、A1… 格子映射、sheetId 与下一页。thumb 读缩略图；detail 直接解码原图且最多 3×3。"
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
                "Lilith Artbank 本地图库。当前只有只读工具：浏览、搜索、查看详情与拼图；不能修改、撤销或导入。",
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
}

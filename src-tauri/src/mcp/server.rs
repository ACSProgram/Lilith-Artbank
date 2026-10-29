use std::time::Instant;

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
    audit,
    context::ReadContext,
    tools::{
        self, ToolOutcome,
        feature::{self as feature_tool, TempFeaturesInput},
        folder::{self as folder_tool, FoldersModifyInput},
        history::{HistoryListInput, HistoryRunInput},
        import::{self as import_tool, ImportStartInput},
        read::{
            self, FindSimilarInput, FoldersListInput, ImageSheetInput, ImagesGetInput,
            SearchImagesInput, TagsListInput,
        },
        tag::{self as tag_tool, TagsModifyInput},
        task::{self, TasksCancelInput, TasksGetInput},
        write::{
            self, CollectionsCreateInput, CollectionsMembershipInput, CollectionsModifyInput,
            ImagesMoveInput, ImagesTrashInput, ImagesUpdateInput,
        },
    },
};

/// 已注册的工具数，由 `library_info` 报给客户端；测试保证它与路由一致。
const TOOL_COUNT: usize = 24;

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

    fn mode(&self) -> &'static str {
        self.app
            .state::<AppState>()
            .settings
            .snapshot()
            .mcp
            .mode
            .as_str()
    }

    /// 在阻塞线程里跑一次工具逻辑：取当前图库快照，再交给领域函数。
    async fn with_context<T, F>(
        &self,
        tool: &'static str,
        operation: F,
    ) -> Result<AppResult<T>, ErrorData>
    where
        T: Send + 'static,
        F: FnOnce(&ReadContext, &AiRuntime) -> AppResult<T> + Send + 'static,
    {
        let started = Instant::now();
        let mode = self.mode();
        let app = self.app.clone();
        let joined = tauri::async_runtime::spawn_blocking(move || {
            let state = app.state::<AppState>();
            let context = state.mcp_context()?;
            operation(&context, &state.ai)
        })
        .await;
        let elapsed = started.elapsed().as_millis();
        let result = match joined {
            Ok(result) => result,
            Err(error) => {
                // 线程没能跑完（panic 或运行时关闭）也要留一条审计。
                let message = format!("MCP 工具执行失败：{error}");
                audit::tool(tool, mode, elapsed, Err(&message));
                return Err(ErrorData::internal_error(message, None));
            }
        };
        match &result {
            Ok(_) => audit::tool(tool, mode, elapsed, Ok(None)),
            Err(error) => audit::tool(tool, mode, elapsed, Err(&error.to_string())),
        }
        Ok(result)
    }

    /// 写工具与任务工具需要完整的 `AppState`（任务总线、确认队列、图库句柄）。
    async fn with_state<F>(
        &self,
        tool: &'static str,
        operation: F,
    ) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(&AppState) -> AppResult<ToolOutcome> + Send + 'static,
    {
        let started = Instant::now();
        let mode = self.mode();
        let app = self.app.clone();
        let joined = tauri::async_runtime::spawn_blocking(move || {
            let state = app.state::<AppState>();
            operation(&state)
        })
        .await;
        let elapsed = started.elapsed().as_millis();
        let result = match joined {
            Ok(result) => result,
            Err(error) => {
                let message = format!("MCP 工具执行失败：{error}");
                audit::tool(tool, mode, elapsed, Err(&message));
                return Err(ErrorData::internal_error(message, None));
            }
        };
        match &result {
            Ok(outcome) => audit::tool(tool, mode, elapsed, Ok(Some(&outcome.value))),
            Err(error) => audit::tool(tool, mode, elapsed, Err(&error.to_string())),
        }
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
        description = "图库概况：名称、路径、图片/回收站/临时文件夹计数、schema 版本、特征数量、AI 配置与 MCP 限制。只读。"
    )]
    async fn library_info(&self) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context("library_info", |context, _| {
                read::library_info(context, TOOL_COUNT)
            })
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "列文件夹及其直接/子树图片数。parentPath 省略时列根目录；给相对路径时列它的直接子文件夹。同级超过 limit 条时返回 nextCursor，原样传给 cursor 取下一页；total 是这一层的总数。新建、重命名、移动、删除文件夹用 folders_modify。"
    )]
    async fn folders_list(
        &self,
        Parameters(input): Parameters<FoldersListInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context("folders_list", move |context, _| {
                read::folders_list(context, &input)
            })
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "管理文件夹：action=create 用 path 新建（缺失的上级一起建）；action=rename 用 folderId+name 改名；action=move 用 folderId 加 parentPath 或 parentId 换上级；action=delete 把整棵子树移入回收站（含子文件夹与图片，原文件留在原路径，可在界面恢复）。临时文件夹、根目录、已在回收站或正在释放的文件夹不能改名、移动或删除。先 dryRun 看 willChange、图片数与子文件夹数；询问档下 rename/create 直接执行，move/delete 需要用户确认。全部写操作记录，可以撤销。"
    )]
    async fn folders_modify(
        &self,
        Parameters(input): Parameters<FoldersModifyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("folders_modify", move |state| {
            folder_tool::folders_modify(state, &input)
        })
        .await
    }

    #[tool(
        description = "列标签及图片数；query 按名称模糊匹配，limit 默认 100、最多 500。total 是命中总数，超过 limit 时返回 nextCursor，原样传给 cursor 取下一页。没有图片的标签不会返回。要重命名、删除或合并标签用 tags_modify。"
    )]
    async fn tags_list(
        &self,
        Parameters(input): Parameters<TagsListInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context("tags_list", move |context, _| {
                read::tags_list(context, &input)
            })
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "管理标签：action=rename 用 tagId+name 改名（撞同名直接报错）；action=delete 删除标签并从所有图片上移除它；action=merge 把 tagId 的图片全部并到 targetId，然后删掉 tagId 这个标签。图片本身不会被删除，三种动作都写操作记录、可以撤销。先 dryRun 看数量；delete/merge 可传 expectedImages 核对源标签当前图片数，不一致直接拒绝。询问档下 rename 直接执行，delete 与 merge 需要用户确认。"
    )]
    async fn tags_modify(
        &self,
        Parameters(input): Parameters<TagsModifyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("tags_modify", move |state| {
            tag_tool::tags_modify(state, &input)
        })
        .await
    }

    #[tool(description = "列集合及图片数，按用户排序。")]
    async fn collections_list(&self) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context("collections_list", |context, _| {
                read::collections_list(context)
            })
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "结构化搜索图片，返回总数、紧凑行与下一页游标；pageSize 范围 1-50，超出直接报错。view 只有 all/trash/collection/temp；要跨回收站搜索请用 searchScope=library。search 匹配描述、标签名与文件夹名。includeFacets 会额外计算分面计数，较慢，按需开启。"
    )]
    async fn search_images(
        &self,
        Parameters(input): Parameters<SearchImagesInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context("search_images", move |context, _| {
                read::search_images(context, &input)
            })
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "找相似：mode=image 用参考图与 kind（phash/color4/color8/embedding/pose）；targetIds 可传多张参考图，但多张只支持 color4/color8/embedding 的平均，最多 100 张。mode=text 是语义搜索，请用英文短语描述画面（例如 a woman in a red cloak, side view）：中文也能用，但命中质量明显下降。scope 可选 library/folder/collection/selection/view/temp，默认 Entire library。threshold 一律是上限：pHash 是 0-256 的距离上限，color4/color8 是 0-1 的距离上限，embedding/pose 是 -1 到 1 的相似度下限（距离越小越像、相似度越大越像，所以 embedding/pose 传 1 会什么都命中，color 传 0 会什么都筛掉）。total 是范围与阈值过滤后的全部命中数，returned 是可翻页/全选的条数（受 limit 截断）。scoreKind=distance 时 score 越小越近，similarity 时越大越近。结果只有一个结果槽，下一次 find_similar 会让当前 token 失效。"
    )]
    async fn find_similar(
        &self,
        Parameters(input): Parameters<FindSimilarInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context("find_similar", move |context, ai| {
                read::find_similar(context, ai, &input)
            })
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "按 id 批量取详情、标签、描述与能力标记；一次最多 50 张。tags 是 [{id,name}]（搜索结果行里的 tags 只是名称数组）。临时区图片的 folderPath 显示为「临时」，并带 temporary=true。"
    )]
    async fn images_get(
        &self,
        Parameters(input): Parameters<ImagesGetInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context("images_get", move |context, _| {
                read::images_get(context, &input.ids)
            })
            .await?;
        Ok(tool_value(result))
    }

    #[tool(
        description = "把 selector 的前 N 张图拼成 1-5 格拼图，返回图片、A1… 格子映射、sheetId 与下一页。格子标签始终按新拼图布局编号；以拼图格子为来源时用 sourceLabel 记录来源。cells[].status 区分 image（有图）、missing（有图但读不出缩略图/原图）、empty（这一格没有图）；被 excludeCells 排除的格子在 excludedCells 里。thumb 读缩略图；detail 直接解码原图且最多 3×3。"
    )]
    async fn image_sheet(
        &self,
        Parameters(input): Parameters<ImageSheetInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .with_context("image_sheet", move |context, ai| {
                read::image_sheet(context, ai, &input)
            })
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
        self.with_state("images_update", move |state| {
            write::images_update(state, &input)
        })
        .await
    }

    #[tool(
        description = "把 selector 解析的图片移动到目标文件夹（后台任务）；目标文件夹不存在时会自动建出整条路径。选了临时区图片会直接拒绝。返回 taskId，完成后任务快照带 operationId。正式写入需 confirmedCount；询问档超过 100 张需确认。已经在该文件夹的图片算跳过：任务快照里 completed 是已处理张数（含跳过），skipped 是其中没有移动的张数。"
    )]
    async fn images_move(
        &self,
        Parameters(input): Parameters<ImagesMoveInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("images_move", move |state| {
            write::images_move(state, &input)
        })
        .await
    }

    #[tool(
        description = "把 selector 解析的图片移入回收站；不会删除原图。临时区图片不能移入回收站，选了会直接拒绝。正式写入需 confirmedCount；询问档不论张数都要用户确认。"
    )]
    async fn images_trash(
        &self,
        Parameters(input): Parameters<ImagesTrashInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("images_trash", move |state| {
            write::images_trash(state, &input)
        })
        .await
    }

    #[tool(
        description = "恢复 selector 解析的回收站图片。正式写入需 confirmedCount；询问档超过 100 张需确认。"
    )]
    async fn images_restore(
        &self,
        Parameters(input): Parameters<ImagesTrashInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("images_restore", move |state| {
            write::images_restore(state, &input)
        })
        .await
    }

    #[tool(
        description = "创建集合，可选把 selector 解析的图片一起加入（同一次调用只产生一条记录）。带 selector 时正式写入需 confirmedCount；超过 100 张在询问档需确认。需要向用户展示一组查找结果时，优先用集合而不是复制文件。"
    )]
    async fn collections_create(
        &self,
        Parameters(input): Parameters<CollectionsCreateInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("collections_create", move |state| {
            write::collections_create(state, &input)
        })
        .await
    }

    #[tool(
        description = "重命名或删除集合。删除可带 expectedItems 核对集合当前图片数；不一致直接拒绝。"
    )]
    async fn collections_modify(
        &self,
        Parameters(input): Parameters<CollectionsModifyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("collections_modify", move |state| {
            write::collections_modify(state, &input)
        })
        .await
    }

    #[tool(
        description = "把 selector 解析的图片加入或移出集合。正式写入需 confirmedCount；询问档超过 100 张需确认。"
    )]
    async fn collections_membership(
        &self,
        Parameters(input): Parameters<CollectionsMembershipInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("collections_membership", move |state| {
            write::collections_membership(state, &input)
        })
        .await
    }

    #[tool(
        description = "列操作记录，包含状态、影响数、从这条开始撤销/重做会动几条、是否可执行与阻塞原因；kind 可过滤。顶层 canRun 表示返回的条目里是否真有可执行的那一条，单条能不能执行看条目的 canUndo/canRedo。下一次写入会删除所有灰色（可重做）记录，撤销后看到记录消失是这个原因。"
    )]
    async fn history_list(
        &self,
        Parameters(input): Parameters<HistoryListInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("history_list", move |state| {
            tools::history::history_list(state, &input)
        })
        .await
    }

    #[tool(
        description = "撤销到指定记录：撤销该记录及之后所有 applied 记录。先 dryRun 看 steps/affected，正式执行必须带一致的 confirmedSteps 与 confirmedAffected；询问档总需确认；撤销链含压缩记录会被明确拒绝，不能绕过。撤销后会产生灰色可重做记录，但下一次写入会清空它们。"
    )]
    async fn history_undo(
        &self,
        Parameters(input): Parameters<HistoryRunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("history_undo", move |state| {
            tools::history::history_undo(state, &input)
        })
        .await
    }

    #[tool(
        description = "重做当前状态到指定记录之间的连续 undone 记录。先 dryRun 看 steps/affected，正式执行必须带一致的 confirmedSteps 与 confirmedAffected；询问档总需确认。灰色（可重做）记录会被下一次写入删除；撤销后再写入，这些记录就不再存在，history_redo 会报「操作记录不存在」。"
    )]
    async fn history_redo(
        &self,
        Parameters(input): Parameters<HistoryRunInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("history_redo", move |state| {
            tools::history::history_redo(state, &input)
        })
        .await
    }

    #[tool(
        description = "查询任务或待确认单。不带 id 时返回活动任务、最近结束的任务与待确认列表；waitMs 最多 30000。用 approvalId 查询会等待用户批准/拒绝并附上关联任务。导入任务额外附带 imported/duplicates/merged/previewIds 回执与 target；includePairs=true 才会返回重复对、批内重复、合并目标与源文件映射数组，默认只给精确计数。三个「重复」口径不同：duplicates 是按重复策略被跳过的源文件数，duplicatePairCount 是命中的库内重复对（merge 策略下 duplicates 为 0 但重复对仍计数），merged 是被合并进已有图片的源文件数；tempReplaced 是图库导入时丢弃掉的临时区同内容行数。completed 在导入任务里是已处理文件数，在移动任务里是已处理张数，移动的 skipped 是其中没有移动的张数。通过 MCP 导入的图片没有 embedding 与描述，语义搜索找不到它们，需要用户在界面上跑批量 AI 任务。"
    )]
    async fn tasks_get(
        &self,
        Parameters(input): Parameters<TasksGetInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("tasks_get", move |state| task::tasks_get(state, &input))
            .await
    }

    #[tool(
        description = "取消 MCP 自己发起的任务；传 approvalId 等同于拒绝这次待确认操作。用户从界面启动的任务不能取消。"
    )]
    async fn tasks_cancel(
        &self,
        Parameters(input): Parameters<TasksCancelInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("tasks_cancel", move |state| {
            task::tasks_cancel(state, &input)
        })
        .await
    }

    #[tool(
        description = "从本机绝对路径导入图片或目录，固定不压缩，也不接受 workerThreads。路径不存在会在入队前直接拒绝。target=library 导入图库并可用 history_undo 撤销；同内容的临时区图片会从临时区消失、改为入库。target=temp 导入临时文件夹，平铺、只跳过重复、不写操作记录，关闭/切换图库或退出应用时清空。返回 taskId；tasks_get 完成后附带导入回执与 previewIds。重复策略在导入前整批选定，导入前无法预知哪些源文件会撞库；完成后用 tasks_get(includePairs=true) 查看源路径与对应图片。通过 MCP 导入的图片没有 embedding 与描述，语义搜索找不到它们，需要用户在界面上手动跑批量 AI 任务。询问档对图库导入每次都要用户确认，导入临时文件夹不需要确认；只读档拒绝。"
    )]
    async fn import_start(
        &self,
        Parameters(input): Parameters<ImportStartInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("import_start", move |state| {
            import_tool::import_start(state, &input)
        })
        .await
    }

    #[tool(
        description = "为临时文件夹中的图片生成 embedding 或姿势特征（kind=embedding|pose），单次最多 100 张。只接受临时图片 id；混入不存在、已删除或普通图库图片时整批拒绝。不生成描述、不写操作记录、不需要 confirmedCount。临时区关闭即清空；先看 library_info.features，库内没有对应特征时搜索结果为空。询问档每次确认，只读档拒绝。返回 taskId。"
    )]
    async fn temp_features_generate(
        &self,
        Parameters(input): Parameters<TempFeaturesInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.with_state("temp_features_generate", move |state| {
            feature_tool::temp_features_generate(state, &input)
        })
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
                "Lilith Artbank 本地图库。可浏览、搜索、查看详情与拼图，也可批量改评分/标签/描述、移动、移入回收站、维护集合与标签、从本机导入图片，以及查看与撤销/重做操作记录。需要向用户展示一组查找结果时，优先创建集合；不要复制文件。导入固定不压缩。\
\n\n当前权限档以 library_info 的 mcp.writeAllowed 与 mcp.approval 为准，三档区别只有一处：\
\n- readOnly：writeAllowed=false，全部写入、导入与撤销/重做都返回“当前是只读模式”。写工具仍然出现在 tools/list 里，这是正常的。\
\n- ask：写入会执行，但 import_start（target=library）、images_trash、history_undo、history_redo、folders_modify 的 move/delete、tags_modify 的 delete/merge，以及任何超过 100 张的批量写入会先进入应用内待确认队列，返回 status=pendingApproval 与 approvalId；用 tasks_get(approvalId) 等用户决定。\
\n- full：不弹确认，直接执行；confirmedCount、confirmedSteps 等数量核对照常保留。approval.threshold 在完全档是 null，意思是这里没有张数阈值，不是“阈值消失后不受限制”。\
\n100 张阈值只在询问档生效，不是设置项；询问档之外的档位不受它约束。\
\n没有永久删除、清空回收站、压缩、像素编辑工具。临时区可以生成 embedding 或姿势特征，但必须先在 library_info 确认库内已有对应特征；普通图库不开放批量 AI 生成。\
\n列表工具的 nextCursor 请原样回传，不要自己拼。",
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
            "import_start",
            "temp_features_generate",
            "folders_modify",
            "tags_modify",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "缺少工具 {expected}"
            );
        }
    }
}

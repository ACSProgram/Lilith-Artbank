# AI 阅读引导

代码任务的第一入口。按问题选择一条路由，只读对应契约与实现；只有跨模块契约变化时才补读相邻模块。

| 问题 | 起点文档 | 实现位置 |
| --- | --- | --- |
| 设置、图库创建/打开/关闭、schema、临时文件夹 | `docs/modules/library.md` | `src-tauri/src/app/`、`src-tauri/src/library/`（临时文件夹在 `library/temp.rs`） |
| 导入、BLAKE3、pHash、主色、缩略图包 | `docs/modules/import.md` | `src-tauri/src/import/`、`src-tauri/src/imageops/` |
| 网格/列表、分页、搜索、筛选、排序、集合、回收站、元数据编辑、文件夹概览与统计缓存 | `docs/modules/browse-and-query.md` | `src/modules/library/`（数据变化与失效计划在 `dataChange.ts`，统计缓存在 `statsStore.ts` / `StatsProvider.tsx`，文件夹概览在 `FolderInspector.tsx`）、`src-tauri/src/library/query.rs`、`folders.rs`、`stats.rs`、`metadata.rs`、`collections.rs` |
| 操作记录、撤销、待释放文件 | `docs/modules/library.md`（操作记录与撤销） | `src-tauri/src/library/history.rs`、`src-tauri/src/app/state.rs`、`src/modules/library/OperationHistoryPanel.tsx` |
| 任务并发、资源锁、图像工作预算、进度计量 | `docs/modules/library.md`（任务与并发）、`docs/guides/performance.md` | `src-tauri/src/concurrency/`（`resources.rs` / `budget.rs` / `meter.rs`）、`src-tauri/src/app/tasks.rs`、`src-tauri/src/app/state.rs` |
| 查看器、裁剪/旋转/调尺寸 | `docs/modules/viewer-and-editor.md` | `src/modules/viewer/`、`src-tauri/src/imageops/` |
| description / embedding / pose、服务进程、语义搜索 | `docs/modules/ai-service.md` | `src-tauri/src/ai/`、`src/modules/ai/`、`server/` |
| 找相似、相似分组、pHash/主色/embedding/pose 相似度 | `docs/modules/ai-service.md`（本地检索）、`docs/modules/browse-and-query.md`（找相似与相似分组） | `src-tauri/src/library/similarity.rs`、`src-tauri/src/ai/pose_similarity.rs`、`src-tauri/src/ai/embedding.rs`、`src/app/useSimilarityBrowse.ts`、`src/modules/library/FindSimilarPanel.tsx`、`src/shared/scope.ts` |
| MCP 服务、stdio 小桥、读写工具、三档权限、确认队列、selector、拼图 | `docs/modules/mcp.md` | `src-tauri/src/mcp/`、`src-tauri/src/imageops/contact_sheet.rs`、`src/app/McpPanel.tsx`、`src/app/McpApprovalCard.tsx` |
| 清理、缩略图重建、描述导入导出、图片空间优化（估算与执行） | `docs/modules/maintenance.md`（图片空间优化） | `src-tauri/src/` 对应领域模块；空间优化在 `imageops/optimize.rs`、`library/optimize_estimate.rs`、`library/optimize.rs`、`src/app/OptimizeDialog.tsx` |
| 构建、检查、基准 | `docs/guides/validation.md`、`docs/guides/performance.md` | 工具脚本与测试 |
| 日志等级、行格式、命令记录、轮换 | `docs/guides/logging.md` | `src-tauri/src/app/trace.rs`、`src-tauri/src/lib.rs` |
| 当前状态/未完成事项 | `docs/planning/current-handoff.md`、`docs/planning/todo.md` | 只读 |

## 读序

1. 先读所选模块文档与 `docs/planning/current-handoff.md`；未完成事项查 `todo.md`。
2. 再读前端 `types.ts` / `api.ts` 或 Rust 的 DTO/命令，确认边界。浏览/查询先看 `src/modules/library/types.ts`、`api.ts` 与 `src-tauri/src/library/query.rs`。
3. 最后读实现。数据库工作进入 `src-tauri/src/library/`；像素工作进入 `imageops/`；AI 只进入 `ai/`；剪贴板等平台差异进入 `platform/`。

## 边界

- 前端模块之间不互相导入；跨模块流程在 `src/app/` 编排。
- `src/modules/<module>/api.ts` 是该领域命令的唯一入口；页面组件不直接 `invoke`。
- Tauri 命令只做适配；业务函数放在领域模块。
- SQLite 写入只走单写连接；不要从读连接写库。
- 颜色只在 `src/styles/tokens.css` 定义（`:root` 浅色 / `html[data-theme="dark"]` 深色）；`index.css` 只引用语义变量，不写字面色值。主题切换走 `src/app/theme.ts`，不要在组件里判断主题。
- 破坏性操作的确认文案走 `src/shared/confirm.ts`，不要在调用点各写一套标题。
- 程序根目录、`settings.json` 与 `logs/` 的解析在 `app/paths.rs`；不要在其他模块各自拼 AppData 路径。
- AI 服务端只改 `server/`；`/v1` 契约变化时同步 `docs/modules/ai-service.md`。
- MCP 只做适配：工具在 `src-tauri/src/mcp/`，拼图像素在 `imageops/contact_sheet.rs`；不复制 `library/`、`ai/` 的逻辑，也不另起图库写进程。
- 任何全量载入/全量扫描方案先对照 `docs/guides/performance.md`。
- 真实仓库 `G:\Lilith Image Library` 只读；测试用临时目录。
- 不建版本号/CHANGELOG/法务文件。

## 变更规则

入口或契约变化时更新 `docs/architecture/` 或 `docs/modules/` 的对应段落；未完成工作写 `todo.md`，当前能力与基线写 `current-handoff.md`。不再有效的计划与批次记录移入 `planning/archive/`，不新建阶段计划文件。

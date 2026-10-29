# Lilith Artbank 全量代码审查报告

- 审查日期：2026-10-29；复核日期：2026-10-29
- 仓库：`F:\Programs\Lilith Artbank`，分支 `main`；初版基线 `0c41553`，本版复核基线 `c80f6cf`（无窗口验收 B1–B3 之后）
- 审查范围：Rust `src-tauri/src` 100 个文件 / 约 6.8 万行，另有 17 个验收测试文件；前端 126 个文件（TS/TSX 约 3.27 万行 + CSS 约 0.78 万行）；`server/` Python 服务；`docs/`；构建与检查脚本
- 审查方式：6 路并行只读审查（库层、应用/并发层、导入/图像/AI/MCP、前端图库层、前端外壳/查看器/样式、文档一致性）+ 主审对高危结论逐条复核；本版按 `c80f6cf` 重新核对 P0/P1、死代码与文档条目，删掉已由 B1–B3 修掉或证伪的结论
- **未做**：未运行 `cargo build/clippy/test`，未运行前端构建与测试，未启动应用，未做真实窗口或 39 万行库实测；B3 自身的自动检查结果见 `docs/planning/current-handoff.md`
- **行号说明**：B1–B3 后 `app/state.rs`、`mcp/approval.rs` 等文件有行号漂移，引用行号只作定位线索，优先按函数名/符号查找
- **复核后的补充**：B1–B3 已把无窗口验收、共享日志装配、开库残留清理等落地；本版不再把它们列为待清理对象。复核同时补进了初版漏掉的 L6–L10、字节预算与结构重复项（见 1.4、第 8 节）

---

## 0. 结论速览

### 0.1 总体判断

这套代码的**骨架是健康的**：单写连接、只读连接池、键集游标、内容寻址、可重建派生数据、`DataChange` 统一失效、`api.ts` 唯一命令入口、资源表分车道，这些关键设计都在，且大多数没有被绕开。

真正的问题不在“有没有架构”，而在**边界处的生命周期没有闭合**：

1. **磁盘动作与数据库事务没有统一补偿**。多个写路径在 SQL 事务里直接 `rename/remove/move` 文件，提交失败或进程中断后，磁盘与数据库会分叉，且没有自动恢复路径。
2. **任务取消/收尾存在竞态和“假终态”**。关库与空闲回收、导入 panic 与写线程、缩略图连续失败与“完成”、不可取消任务与“请取消”的提示互相矛盾。
3. **前端异步边界缺少身份校验**。切图、切库、切范围后旧请求/旧缓存仍能落地，典型是详情栏显示并写入上一张图、缩略图跨库串图、刷新保留行时漏掉页边界。
4. **巨型文件把多个关注点焊在一起**。`history.rs` 6437 行、`query.rs` 5085 行、`AppState` 所在的 `state.rs` 2901 行、`LibraryView.tsx` 2770 行、`index.css` 7835 行，导致任何小修都要跨多个区域。
5. **补丁式重复开始堆积**。任务进度 UI 五套、右键菜单三套、虚拟滚动两套、文件夹树聚合五份、ID/路径校验三套、冲突文案三处；同时存在若干确实不再可达的枚举/字段/表。

### 0.2 严重度定义

| 级别 | 定义 |
| --- | --- |
| P0 | 可能造成磁盘与数据库不可逆分叉，或任务已结束后仍继续写库；触发概率不高但后果重 |
| P1 | 明确缺陷或高风险，触发条件常见、恢复成本高，或直接造成用户可见的数据/状态错误 |
| P2 | 结构性问题、中等概率缺陷、性能浪费，或明显增加后续改动成本 |
| P3 | 清洁度、文档、文案、局部性能与可维护性 |

### 0.3 最该先处理的 10 项

| # | 问题 | 位置 | 级别 |
| --- | --- | --- | --- |
| 1 | 多文件夹撤销/重做把多次磁盘 rename 放进一个事务，后一项失败时数据库回滚但前面的目录已改 | `library/history.rs:3110-3319`、`library/folders.rs:1445-1497` | P0 |
| 2 | 导入工作线程 panic 时写线程不会被 join，任务/操作记录收尾后写线程仍可能入库 | `import/pipeline.rs:256-330`、`import/mod.rs:195-211` | P0 |
| 3 | 关库“取消任务 → 取会话锁”之间存在窗口，空闲回收可提交排队任务并在关库后继续跑旧库 | `app/state.rs:858-899`、`app/idle.rs:121-142`、`app/tasks.rs:1055-1085` | P1 |
| 4 | 文件系统动作发生在 `history::write` 事务内，提交失败没有补偿（移动/编辑/孤儿清理） | `library/metadata.rs:1260-1377`、`library/edit.rs:203-328`、`library/maintenance_actions.rs:89-154` | P1 |
| 5 | 撤销暂存目录清理用读连接，可能删掉另一个未提交编辑的 `.artbank/undo/<id>` | `library/history.rs:1400-1427`、`app/state.rs:1530-1550` | P1 |
| 6 | 待释放状态机不闭合：释放失败被吞、非法路径行永远删不掉、非空目录让 `purge_operation_id` 永久为真 | `library/metadata.rs:1670-1719,1749-1808`、`library/history.rs:4628-4640` | P1 |
| 7 | 刷新保留旧行时按 `previousCount` 截断，页边界之后会永久漏一行 | `src/modules/library/useImageQuery.ts:123-188` | P1 |
| 8 | 详情栏切图不先清空 `detail`，请求返回前显示上一张，写操作可能落到上一张 | `src/modules/library/Inspector.tsx:313-339,420-447` | P1 |
| 9 | 缩略图 URL 只有 `imageId + version`，切库后同 id 同版本的缩略图会跨库命中 | `src/shared/thumbUrl.ts:33-41`、`src/shared/thumbCache.ts:16`、`app/thumb_protocol.rs:11,74` | P1 |
| 10 | 字节预算只约束导入；优化、估算、查看器、AI 解码、MCP 拼图只受线程数约束 | `concurrency/budget.rs`、`library/optimize.rs:675`、`library/viewer.rs:117,184`、`mcp/sheet.rs:269,286` | P1 |

复核后优先级有两处调整：L9（路径大小写可让文件夹移进自己的子树）并入数据安全首批；P1 第 10 项初版没有落进任何批次，本版单列。其余级别维持初版判断。

---

## 1. 架构合理性

### 1.1 做对的部分（不建议重构掉）

- **写路径收敛**：`LibraryDb` 单写连接 + `IMMEDIATE` 事务 + 读连接 `query_only`，未发现从读连接写库。
- **查询不搬全量**：键集游标与 id 列表游标并存，前端只持有当前页与选择集；`DataChange` → `planDataChange` 是失效决策的单一入口。
- **领域分层基本成立**：`imageops/` 只做像素，`ai/` 只做 HTTP 与任务，`platform/` 收平台差异，`mcp/` 做适配。
- **前端命令入口**：绝大多数页面经 `src/modules/<module>/api.ts`，没有大面积直接 `invoke`。
- **文档与代码的契约意识**：版本、schema、阈值、上限大多能在文档与代码之间对上（详见第 7 节）。

### 1.2 主要结构问题

#### A. `AppState` 是上帝对象，app 层出现 SQL 与 DTO 映射

`app/state.rs:80-104` 同时持有：图库会话、任务总线、资源表、AI 运行时、MCP 确认队列与拼图缓存、导入句柄/回执、估算缓存、检查报告缓存、空闲线程。app 层还直接写 SQL（`state.rs:1379-1406,1438-1456,2652-2677,2798-2856`）并做 DTO 映射（`state.rs:2786-2792`）。

同时，`commands.rs` 在 viewer/edit/AI 三处绕过 `AppState` 直接调领域函数（`commands.rs:918-936,965-980,1105-1186`），导致同一契约有三套入口：`AppState` 方法、命令直调、MCP 工具。新增一个入口要同时找三处。

**建议**：先不追求大拆，按职责收口四个面：
- `LibrarySession`：open/close/counts/句柄；
- `HistoryService`：undo/redo/clear/release；
- `MaintenanceTasks`：检查/导出/回收/优化；
- `McpGateway`：确认队列与拼图缓存。

app 层禁止出现 `connection.prepare`；viewer/estimate/AI 的编排收回 `AppState` 方法。

#### B. 库层的“同一概念，多份实现”

- **文件夹祖先/子树聚合 5 份**：`query.rs:2470-2496 expand_folder_counts`、`:1130-1159 list_folders`、`:1162-1225 list_trash_folders`、`:983-1050 folder_stats`、`:1066-1102 subfolder_count`，另有 `folders.rs:534-561 subtree_folder_ids/ancestor_folder_ids`。
- **ID 校验 3 套**：`metadata.rs:1960`、`collections.rs:724`、`query.rs:1698/964`。
- **路径守卫 3 套**：`folders.rs:134 ensure_inside_root`、`metadata.rs:1833 ensure_dir_inside_root`、`metadata.rs:1853 ContentRemover`。
- **同义查询**：`query.rs:528 rows_by_ids` 与 `:563 rows_for_ids` 主体相同；`:464 build_sorted_ids` 与 `:630 query_ids` 的 SQL 基本相同。
- **事务样板 33 处 `transaction_with_behavior`、41 处 `with_writer`**，每个 undo/redo 自己开事务、自己按 `ChunkFormat` 过滤解码。

这类重复直接产生了下面的真实缺陷：路径大小写口径在 SQL `NOCASE` 与 Rust `starts_with` 之间分裂（见 4.9）。

**建议**：
- 新建 `library/ids.rs`（上限、去重、错误文案）与 `paths::ensure_within(root, path)`；
- 建一次读取的 `FolderTree { by_id, by_path, children }`，统一 `ancestors/subtree/counts`，query 与 folders 都消费它；
- 抽 `history::with_tx` 与 `history::json_chunks<T>(tx, op_id)`，kind → (undo, redo) 做成两张分派表；
- 所有长任务统一用 `LiveOperation`，禁止手写 `INSERT INTO operations`。

#### C. 任务通道有两条，语义各维护一份

`app/commands.rs:680-717` 的 `run_write_task` 自己拼 `TaskHandle + begin_queued + set_phase(Running) + catch_unwind + finish_from`，与 `app/tasks.rs:949-963` 的 `spawn_worker/TaskRunGuard` 是两套实现。准入、相位、panic 文案、终态逻辑各维护一份，后续改任务语义容易漏改。

**建议**：下沉为 `TaskBus::begin_blocking_task(...)`，统一复用 `spawn_guarded`/`TaskRunGuard`；commands 只做 DTO 解析。

#### D. 前端 `shared/` 出现分层倒挂

`shared/confirm.ts:1`、`shared/components/ConfirmDialog.tsx:1` 反向依赖 `app/noticeContext`；`shared/scope.ts:1`、`batchTaskContext.ts:1`、`capability.ts:1`、`similarity.ts:1`、`viewerSession.ts:2` 依赖 `modules/library`；`app/types.ts` 的 `UiSettings` 被多个模块使用（`LibraryView.tsx:46`、`VirtualGrid.tsx:21`、`VirtualList.tsx:23`、`SortMenu.tsx:3`），而 `ImageViewer.tsx:26`、`ImportDialog.tsx:13` 又反向依赖 `app/noticeContext`。

`shared/` 名义上是无领域语义的底层，实际上已经依赖 app 与 library，单测与拆包都会遇到循环/隐式全局。

**建议**：
- `NoticeApi/ConfirmOptions` 移到 `shared/notices.ts`；
- 设置类型移到 `shared/settings.ts`；
- 任务/范围/查看器契约移到 `shared/contracts/`；
- 模块 UI 通过 props/context 注入提示，不直接 import `app/noticeContext`。

#### E. 导入/优化/AI/MCP 的边界

- **导入**：`import/pipeline.rs` 2045 行同时承担编排、线程池、写线程、事务、历史记录与测试；`import/mod.rs:279-289` 每 150ms 推 `import-progress`，`app/tasks.rs:1096-1108` 另起线程推 `task-progress`，两套进度泵与两套百分比口径并存。
- **优化**：`library/optimize.rs`（1876 行）与 `library/optimize_estimate.rs`（2248 行）重复 `SourceInfo`（`optimize.rs:99-109` 与 `optimize_estimate.rs:492-500`）；`empty_estimate`（`optimize_estimate.rs:758-871`）逐字段复制 50+ 项；多处 9 参函数与 `#[allow(clippy::too_many_arguments)]`。
- **AI**：`ai/runtime.rs`（1481 行）混合进程监督、检索、均值缓存与任务编排；均值单飞只靠 15 秒轮询（`runtime.rs:1251-1354`），慢冷启动时可能两个线程同时全包扫描。
- **MCP**：审批三份判定（`tools/write.rs:171-173`、`folder.rs:74-76`、`tag.rs:62-64`），每个写工具重复约 80 行 dryRun/confirmed/approval/finish 骨架；工具说明在 `server.rs:146-475` 与 `:486-495` 各一份。
- **Python 服务**：`server/main.py` 路由、校验与推理混在一起；`/v1/shutdown` 无鉴权（`:130-141`）；`ThreadingHTTPServer` 线程无上限（`:84-92`）；`image_io.py:14-23` 解码无像素上限。

### 1.3 补丁式功能与过度设计

| 项目 | 证据 | 判断 |
| --- | --- | --- |
| `BatchTaskKind::PackCompact` | `app/state.rs:1876` 有分支，`batch_preview.rs:98-106` 直接拒绝；前端只走专用命令 `start_pack_compact`（`src/app/maintenanceApi.ts:22`），全前端没有向 `start_batch_task` 送 `packCompact` | 无效枚举面，删一侧 |
| `TaskLane` / `lane` | `tasks.rs:1-8` 注释称用于“任务中心展示与优先级提示”，`tasks.rs:141-147,333-335,536` 计算并序列化；前端只在 `taskTypes.ts:27,170,211` 声明、`mock.ts:1441` 填值，从未渲染或比较 | 注释夸大，字段无消费者 |
| `ImportRequest.worker_threads` | `import/mod.rs:85-86` 有字段，`app/state.rs:217` 与 MCP 永远传 `None`，`pipeline.rs` 只用全局 `images.current_limit()` | 死参数 |
| `undo_runs` 表 | `schema.rs:201-207` 建表，`history.rs:1726-1745` 只写，唯一读取是 `recover_on_open` 的 `UPDATE ... WHERE state='running'`；`trim/clear` 都不清理 | 只写不读的审计表，且无界增长 |
| `FilterCounts.folders` | `src/modules/library/types.ts:113` 声明；Rust `query.rs:2189-2210,2283-2287` 每次分面都算；`FilterPanel.tsx:97-108` 只消费 extensions/scores/tags | 死字段 + 白付 62–66ms |
| `contextRevision` | `LibraryView.tsx:296` 定义后从未使用 | 死 ref |
| 任务进度 UI ×5 | `TaskCapsule.tsx:24-115`、`BatchTaskDialog.tsx:482-525`、`OptimizeDialog.tsx:142-257`、`MaintenanceDialog.tsx:779-807`、`SimilarityGroupsDialog.tsx:471-485` | 取消状态与文案不一致 |
| 右键菜单 ×3 | `LibraryContextMenu.tsx:78-129`（有键盘导航）、`FolderTree.tsx:808-957`、`ViewBar.tsx:213-283` | 只有一套可键盘操作 |
| 虚拟滚动 ×2 | `VirtualGrid.tsx:74-206,304-328` 与 `VirtualList.tsx:142-251,405-429` 几乎逐行相同 | 抽 `useVirtualWindow` |
| `SortSpec` 两套 | `app/settings.rs:21-34,350-368` 字符串表 vs `library/query.rs:29,95-98,754,1591` 枚举 + 重复的“最多 7 级”校验 | 规则只留查询层一处 |
| 冲突文案三处 | `concurrency/resources.rs:270-276`、`app/tasks.rs:682-688`、`app/state.rs:2641-2646` | 改一句要改三处 |
| 编辑编码两入口 | `imageops/optimize.rs:571-602` 同时保留 `encode_ladder` 与 `encode_ladder_owned` | 只留 owned |

**结论**：这些不是“功能太多”，而是同一能力被不同批次各写了一遍。清理它们不需要新功能，属于低风险高收益的债务回收。

### 1.4 初版批次计划漏掉的部分

- **字节预算只覆盖导入**（P1 第 10 项）：优化、估算、查看器、AI 解码、MCP 拼图都只受线程数约束，初版没有给它们排批次。
- **L6–L10 没有落进任何批次**：导入合并撤销、缺原图重做、恢复图片与文件夹删除的锁、路径大小写、段文件 fsync。
- **Python 服务没有严重度也没有批次**：`/v1/shutdown` 无鉴权、`ThreadingHTTPServer` 线程无上限、`image_io.py` 解码无像素上限。
- **结构重复没有对应批次**：文件夹聚合 5 份、ID 校验 3 套、路径守卫 3 套、任务通道 2 套、MCP 写工具样板。初版的“拆分”只搬代码，不会消除这些重复，反而会把它们搬进更多文件。

---

## 2. 文件结构

### 2.1 Rust 超大文件

| 文件 | 行数 | 其中测试 | 关注点 | 建议拆分 |
| --- | --- | --- | --- | --- |
| `library/history.rs` | 6437 | 约 1777 | 类型/写入、列表/预览/裁剪、可见性、评分/描述、标签、集合、文件夹、移动、编辑、导入、编码 | `mod.rs`（类型 + write/begin_live/finish_live）、`replay.rs`（list/preview/trim/recover/run_undo/run_redo）、`undo/{visibility,metadata,tags,collections,folders,move,edit,import}.rs`、`chunks.rs` |
| `library/query.rs` | 5085 | 约 2468 | 类型/归一化、分页与缓存、SQL 构造、统计、文件夹、标签、待处理 | `mod.rs`、`page.rs`、`sql.rs`、`stats.rs`、`folders.rs`、`tags.rs`、`pending.rs` |
| `app/state.rs` | 2901 | — | 会话、任务、资源、AI、MCP、导入、估算、报告、空闲 | `session.rs`、`history_service.rs`、`maintenance.rs`、`mcp_gateway.rs` |
| `library/metadata.rs` | 2650 | — | 读、字段写入、合并、移动、回收/释放 | `read.rs`、`fields.rs`、`merge.rs`、`move.rs`、`purge.rs` |
| `library/folders.rs` | 2434 | — | 校验、增删改移、排序、回收站、undo 原语、路径辅助 | `mod.rs`、`mutate.rs`、`trash.rs`、`undo.rs`、`tree.rs` |
| `library/optimize_estimate.rs` | 2248 | — | 抽样、分层、外推、报告 | 与 `optimize.rs` 共享 `OptimizeRun` 参数对象与 `Estimate DTO` 默认构造 |
| `import/pipeline.rs` | 2045 | 约 880 | 编排、线程池、写线程、事务、历史、测试 | `pipeline/scan_job.rs`、`writer.rs`、`publish.rs` |
| `library/optimize.rs` | 1876 | — | 候选、执行、提交、删旧、报告 | `optimize/candidates.rs`、`execute.rs`、`commit.rs` |
| `app/tasks.rs` | 1655 | — | 任务总线、相位、快照、资源、批量预览、测试 | 先抽 `TaskHandle` 与 `TaskBus` 到两个文件 |
| `library/perf.rs` | 1593 | — | 基准 | 本轮进行中，暂不动 |
| `mcp/tools/write.rs` | 1568 | — | 写工具样板 | 抽 `WriteGate` 后自然缩小 |

### 2.2 前端超大文件

| 文件 | 行数 | 主要问题 | 建议拆分 |
| --- | --- | --- | --- |
| `src/styles/index.css` | 7835 | 约 1092 条顶层规则；分区仍叫 `S3 views/S3 dialogs/S3 filter additions`；`.viewer-tools` 7 处、`.optimize-legend` 11 处覆盖；约 200 行死规则；3 处字面色值 | `styles/app.css`、`library.css`、`viewer.css`、`dialogs.css`、`maintenance.css`，`index.css` 只 `@import` |
| `src/modules/library/LibraryView.tsx` | 2770 | 约 40 个 state/ref；查询、变更应用、选择、键盘、拖放、工具栏、空态、8 个对话框 | `useLibrarySelection.ts`、`useLibraryChanges.ts`、`LibraryToolbar.tsx`、`LibraryStateOverlay.tsx`、`LibraryDialogs.tsx` |
| `src/modules/library/mock.ts` | 1536 | 与真实查询语义漂移（忽略 `folderDirect/tagPresence/excludeTagIds`，`embedding/pose=has` 恒 false） | 保留，但补齐或标注预览能力边界 |
| `src/modules/library/Inspector.tsx` | 1521 | 外壳 + ImageInspector + BatchInspector + 部件；“详情异步”与 UI 混在一起 | `ImageInspector.tsx`、`BatchInspector.tsx`、`InspectorParts.tsx`、`useImageDetail.ts` |
| `src/app/OptimizeDialog.tsx` | 1415 | 估算/执行/图表/任务订阅混在一起；关闭时仍订阅 | 拆执行面板与图表，任务改用窄订阅 |
| `src/modules/viewer/ImageViewer.tsx` | 1135 | 源图加载、渐进层、GIF、编辑入口、失败重试 | 抽 `useViewerSource` 与 `ViewerStage` |
| `src/modules/library/FolderTree.tsx` | 1013 | 展开、持久化、选择、菜单、对话框、拖放、双窗格；未虚拟化 | `FolderTreeRows.tsx`、`FolderContextMenu.tsx`、`useFolderExpansion.ts`、`useFolderSplit.ts` |
| `src/app/MaintenanceDialog.tsx` | 954 | 四个分区 + 任务 + 报告 | 按分区拆分 |
| `src/app/SettingsDialog.tsx` | 940 | 七个分类 + 草稿 + 状态刷新 | 按分类拆分，草稿抽 hook |
| `src/app/LibraryWorkspace.tsx` | 894 | 20+ state；文件夹/回收站/变化去重/任务上下文/查看器/导入/拖放/相似浏览 | `useLibraryData`、`useTaskDialogs`、`useFolderDrag` |

**拆分原则**：先补测试再拆；按“状态与副作用”而不是按 JSX 行数切；一次只拆一个文件，保持行为不变。`FolderDelete` 等旧记录兼容分支不要顺手删除（见 6.3）。

---

## 3. 后台任务、取消、错误与极端时序（重点）

> 本项目的任务语义已经写得很细（排队、取消中、让位、失败清单、进度计量），但实现里仍有几处“收尾早于收尾对象”“取消不到自己”“终态说错话”。

### 3.1 P0

#### T1. 导入工作线程 panic 时写线程脱离，任务结束后仍可能入库

- 位置：`src-tauri/src/import/pipeline.rs:256-330`、`src-tauri/src/import/mod.rs:195-211`、`src-tauri/src/library/history.rs:867-888`
- 事实：写线程在 `pipeline.rs:261` spawn；`pool.install(...)` 在 `:298-311`，panic 会在这里展开；只有正常路径才在 `:323` `writer.join()`。`run_pipeline` 的 `catch_unwind` 捕获后直接 `finish(...)`，而 `finish_live` 在无 chunk 时会删除 `operations` 行（`history.rs:867-888`）。
- 影响：工作线程（例如 WebP 编码器 panic）展开后，写线程仍持有 receiver 并继续 `append_live_in_tx`。若此前无 chunk，外键失败会让整批回滚；若有 chunk，则出现“任务已失败、行与记录继续增长”的不一致。用户看到失败，但部分图片已经入库或正在入库。
- 建议：写线程改为 `std::thread::scope` 或带 Drop guard 强制 join，保证任何退出路径都在 `finish`/`LiveOperation::finish` 之前排空写线程；`append_live_in_tx` 在记录已不存在时降级为跳过，而不是让整批失败。

#### L1. 多文件夹撤销/重做：磁盘 rename 与 SQL 事务不在同一原子边界

- 位置：`src-tauri/src/library/history.rs:3110-3203`（undo）与 `:3229-3319`（redo）；`src-tauri/src/library/folders.rs:1445-1497 undo_path_change`
- 事实：`undo_folder`/`redo_folder` 在**一个** `IMMEDIATE` 事务里循环处理多个 `FolderPathChange`；每一项的 `undo_path_change` 先 `rename_directory`（`folders.rs:1475`），再 `update_subtree_paths`（`:1479`）。失败时只把当前这一项目录 rename 回去（`:1487-1494`），但此前已经成功的目录 rename 不会回滚，而事务会整体回滚。
- 影响：`place_many` 一次移动多个文件夹会写多条 `FolderPathChange`。第二项因目标被占用/源目录消失失败时：数据库回到旧路径，第一项目录已经在磁盘上改成新路径 → 图片行指向不存在的目录，检查/查看器全部报缺失；重试撤销还会因“当前路径与记录不符”再次失败。
- 建议：每个 `FolderPathChange` 一个独立事务（与 `run_undo` 按记录逐条提交一致），或收集 `done: Vec<(old, new)>`，任一后续步骤失败时逆序 rename 回去。`undo_create`（`folders.rs:1354-1377`）与导入收尾 `cleanup_created`（`:1380-1403`）有同一问题，应一并处理。

### 3.2 P1

#### T2. 关库“取消 → 取会话锁”之间存在竞态，空闲回收可在关库后继续跑旧库

- 位置：`src-tauri/src/app/state.rs:858-899`；`src-tauri/src/app/idle.rs:121-142`；`src-tauri/src/app/tasks.rs:1055-1085`
- 事实：`close_library` 先检查 `active()` 中是否有不可取消任务（`:860-875`），再 `cancel_for_close()`（`:876`），再 `begin_session_write(10s)`（`:878-879`），最后才 `stop_idle_loop()`（`:886`）。空闲线程每 30 秒 tick（无窗口构建可用环境变量缩短），`tick` 先判 `tasks.is_idle()`，再调 `start_pack_compact`。
- 影响：空闲线程在 `is_idle` 判断通过后、`start_pack_compact` 提交前，关库正好执行了 `cancel_for_close`。新提交的 `PackCompact` 会注册并排队在会话锁之后；关库完成后它拿到旧的 `database/thumbs/root` 继续写旧库缩略图包。表现为：关库后被“有任务占用”拒绝再开库，或旧库缩略图包在关库后仍被改写。
- 建议：把 `stop_idle_loop()` 提到第一轮取消之前；`begin_session_write` 等待循环中每轮重跑取消；给 `AppState` 加图库代次（`AtomicU64`），任务启动前校验代次，过期直接 `Cancelled`。

#### T3. 只是“排队中”的任务也会挡住所有短写，提示却说“正在进行”

- 位置：`src-tauri/src/concurrency/resources.rs:496-516`、`src-tauri/src/app/tasks.rs:674-680`、`src-tauri/src/ai/runtime.rs:709-721`
- 事实：`try_acquire` 把所有等待者都当阻塞者；描述任务带 `metadata_writer`。一个描述任务排在别的 AI 任务后面时，评分/标签/集合等短写全部立即失败，提示“「生成描述」正在写入图库内容”，而它一行都没开始。
- 影响：用户会以为故障；短写体验被排队任务拖死。
- 建议：等待者只对冲突的路径写行使优先权，元数据/内容写等粗资源允许短操作插队；至少错误文案区分“排队中”。

#### T4. 自动安排的“释放删除文件”不可取消，却让关库/换库被拒并提示“请取消”

- 位置：`src-tauri/src/app/state.rs:1477-1483`、`:1718-1724`、`:858-875`、`tasks.rs:124-138`
- 事实：`ReleaseGarbage` 统一 `set_cancellable(false)`；它不在 `can_cancel_for_close` 白名单里。导入/裁剪后会自动提交，`Queue` 策略可长期排队。关库时提示“……请等待完成或取消后再关闭图库”，但 `TaskBus::cancel` 会回答“该任务不能中途取消”。
- 影响：用户被要求做一件做不到的事，只能等；期间无法换库。
- 建议：给释放任务加“按文件粒度取消并保留 pending 标记”，或关库时对它走“先取消、保留待释放标记”的特殊路径；文案按是否可取消区分。

#### T5. 缩略图连续失败即中止，却以 `Completed` 收尾

- 位置：`src-tauri/src/app/state.rs:1935-1972`
- 事实：连续 5 次失败 `break`（`:1965-1968`），循环后无条件 `worker.finish_from(&Ok(()))`（`:1972`）。
- 影响：失败 5 张后剩余几千张被跳过，任务胶囊显示“完成”、进度 100%；`history_page` 也当正常结束。对比压缩任务同类情况走 `finish(Failed)`（`state.rs:2217-2221`）。
- 建议：`break` 时记录 `aborted`，循环后 `finish(Failed)`（或专门的“部分失败”终态），并把“未处理 N 张”写进 message。

#### T6. AI 监督线程的 `supervising` 标志会永久卡在 true

- 位置：`src-tauri/src/ai/runtime.rs:1356-1368`、`:1470-1479`、`:486-504`
- 事实：`supervising.swap(true)` 早退；线程持有当前代次 token；guard 只在自己代次仍是当前代次时才清标志。`apply_settings` 先 `fetch_add` 代次再 `spawn_supervisor`，不清标志；只有 `start_service` 会重置。
- 影响：服务启动中（最多 60 秒 + 退避）保存 AI 设置或停止时，旧监督线程因代次变化直接返回，guard 因 token 不匹配拒绝清除；新监督线程被 `swap` 挡掉。此后自动拉起一直静默失败，用户只能手动点“启动”。
- 建议：把监督状态做成 `(generation, running)` 一体或 `Mutex<Option<SupervisorId>>`，换配置时在同一把锁里“作废旧代次并交接”；guard 用 `compare_exchange` 只清自己那一代。

#### T7. AI 请求“300 秒 × 3 次重试”让取消最坏等约 15 分钟

- 位置：`src-tauri/src/ai/client.rs:8-12,278-299,374-387`、`src-tauri/src/ai/job.rs:198-216`
- 事实：describe 单次 300s，最多 3 次尝试，退避 250ms/1s；取消只在图片之间检查。
- 影响：服务卡死时单张描述要占 15 分钟；用户点取消后任务长时间停在“取消中”，并持续占 AI 服务与内容写资源。
- 建议：给 `AiClient` 传入取消/截止时间（每次重试前检查，重试等待可中断），或把单次尝试超时降到 60–120s 并封顶总预算。

#### T8. MCP 确认队列生命周期不闭合：Running 永不过期、队满会挤掉执行中的卡片、任务终态不回写

- 位置：`src-tauri/src/mcp/approval.rs:133-140`、`:266-273`、`src-tauri/src/app/state.rs:543-573`
- 事实：超过 50 条无条件 `pop_front`；只有 Pending 会过期；Running 的终态只从任务快照临时推导，不落回记录。
- 影响：执行线程 panic 或长时间运行后卡片永远“执行中”，`tasks_get(approvalId)` 一直等；新的确认单可能把正在执行的记录挤掉，AI 再查询得到“不存在”。
- 建议：只淘汰终态记录；Running 加心跳/最长执行 TTL 并在超时置 Failed；任务终态时回写记录。

#### T9. 关闭 MCP 只关监听，不撤销已建立连接与令牌；工具也不检查 `enabled`

- 位置：`src-tauri/src/mcp/runtime.rs:210-225`、`:286-323`、`src-tauri/src/mcp/tools/write.rs:161-169`
- 事实：stop 只 cancel accept 循环；每条连接独立 `tokio::spawn`，未跟踪；写工具只判 `ReadOnly`。
- 影响：用户关闭 MCP 后，IDE/客户端保持的 keep-alive 连接仍可继续调用读工具，完全档下甚至继续写库——权限撤回不彻底。
- 建议：记录每条连接的任务句柄并在 stop 时 abort；所有工具入口统一校验 `settings.enabled`（读工具至少校验 enabled）。

#### T10. 缩略图包回收把“垃圾字节”直接计为已释放，删除失败也照记；中途出错丢掉已完成报告

- 位置：`src-tauri/src/library/pack_gc.rs:248-281`、`:462-467`
- 事实：`Compacted(segment.garbage)` 直接累加 `freed_bytes`；删除失败只 warn；`compact_segment` 的 `?` 会把已回收段报告整体丢掉。
- 影响：Windows 上有读者句柄时旧段删不掉，界面与 meta 却记“释放 X”；后一段出错时任务失败且看不到前几段已回收。
- 建议：`freed_bytes` 只在删除成功后累加，失败计入 `delete_failed` 并保留待删清单；出错时返回带部分结果的报告。

#### T11. 压缩的删旧失败对用户不可见，“省 X”可能包含仍在磁盘上的旧原图

- 位置：`src-tauri/src/library/optimize.rs:1182-1235`、`:363-366`、`src-tauri/src/app/state.rs:2191-2216`
- 事实：删除失败只 `report.delete_failed += 1` 与日志；字段只序列化、无人读；任务摘要只用 processed/savings/skipped/failed。
- 影响：旧原图/旧预览被占用时，界面仍显示“省 N GB”；用户据此判断磁盘空间会出错，直到维护页孤儿检查才暴露。
- 建议：把 `delete_failed/deleted_files` 进入任务摘要与完成报告（例如“N 张旧原图删除失败，可在检查图库中清理”），并写入失败清单。

#### T12. 同步命令在 IPC 线程里做全库 id 解析与导入写事务

- 位置：`src-tauri/src/app/commands.rs:1025-1034`、`src-tauri/src/app/state.rs:1886,1999,2419,2482`、`commands.rs:315-323`
- 事实：`start_batch_task` 非 async，`scope.resolve` 解析整个图库 id；`start_import` 同步执行 `LiveOperation::begin`（写事务 + fsync）。代码自己在 `commands.rs:664-666` 已声明同步命令会卡 UI。
- 影响：20 万/39 万张库上点“重建缩略图/估算/导出/检查”会在解析 id 期间冻结 WebView；导入开始也带一次 fsync 卡顿。
- 建议：改成 `async` + `run_blocking`（已有模板），把 `scope.resolve`、`LiveOperation::begin` 一起搬进 `spawn_blocking`。

#### T13. 任务快照跨库残留，进度事件无去抖/背压

- 位置：`src-tauri/src/app/state.rs:759,888-896`、`src-tauri/src/app/tasks.rs:896-905,1096-1109`
- 事实：换/关库清掉估算、检查报告、导入回执、审批，但 `tasks.entries` 不清；每个任务一个 pump 线程，固定 150ms 全量 `snapshot()`（含最多 200 条失败清单）。
- 影响：换库后任务中心仍显示上一个库的任务；多任务并存时事件频率 = 任务数 × 6.7/s，每帧克隆失败清单。
- 建议：`close_library/open_library` 清空已完成任务（或按 library 代次过滤 `list()`）；pump 比较上次快照摘要再推，失败清单按需取。

### 3.3 任务/进度语义的“小而不小”的问题

| 问题 | 位置 | 影响 | 建议 |
| --- | --- | --- | --- |
| 相似分组把 0–1000 的加权值冒充“张数” | `ai/runtime.rs:1143,1432-1465`、`tasks.rs:512-528`、`TaskCapsule.tsx:58-59` | 显示“432 / 1000”像图片张数，速度单位也错 | 用 `meter.set_total_units`，`total` 保持真实张数或只暴露 `progress_percent` |
| 排队任务被算作“正在进行” | `tasks.rs:674-680` | 冲突提示误导 | 文案区分“排队中” |
| 关库先清内存状态、后持久化 `library_path` | `state.rs:888-897` | 写盘失败时命令报错但库已关，下次启动又自动打开 | 先 `set_library_path(None)`，失败只 warn |
| 设置校验失败直接让应用起不来 | `settings.rs:483-511`、`lib.rs:79-80` | 手改坏一个合法 JSON 值就静默不启动 | 校验失败同样备份并回落默认值 |
| 设置先改内存再落盘，失败不回滚 | `settings.rs:517-531,536-546` | 命令报错但进程已按新设置运行 | 先序列化到临时文件再交换，或失败回滚 guard |
| `save_image_edit` 持内容写锁再等图像预算 | `commands.rs:944-958` | 保存编辑可无限期占住所有写与关库 | 先取预算再取资源守卫，并传真实取消闭包 |
| `start_import_inner` 的 `Conflict` 分支不可达 | `state.rs:306-317` | 死分支误导 | 删除，或导入改可选 `Fail` 策略 |

---

## 4. 图片与文件夹状态、生命周期（重点）

> 项目自己的架构文档写明：“写事务只做元数据与指针切换；解码、缩放、编码、哈希都在事务外并行完成。”（`docs/architecture/overview.md:96`）当前有多条写路径违反这条不变量，这是本章多数问题的根因。

### 4.1 P0

- **L1**：见 3.1（多文件夹撤销的磁盘/SQL 错位）。

### 4.2 P1

#### L2. 文件系统动作在 `history::write` 事务内，提交失败没有补偿，四个写路径四种写法

- 位置：
  - `library/metadata.rs:1260-1377` `move_images_with_id`：事务内逐张 `fs::rename`（`:1326`），只在单行 SQL 失败时反向 rename（`:1343-1362`）；
  - `library/folders.rs:254-263`（rename）与 `:366-376`（move_to）：只在闭包内部错误时回滚；
  - `folders.rs:784-787`（place_many）：闭包外按 `result.is_err()` 回滚；
  - `library/edit.rs:295-303`：用 `published` 标志；
  - `library/maintenance_actions.rs:121`：无补偿。
- 事实：`history.rs` 生产代码有 33 处 `transaction_with_behavior`、41 处 `with_writer`；提交期错误（磁盘满、SERIALIZE 失败）不走单行失败分支。
- 影响：文件已移动/已发布而 DB 回滚，出现“行指向旧目录、文件在新目录”；用户看不到提示。
- 建议：给 `history::write` 增加 `on_commit_failure` 回调（或返回必须回滚的 file journal），统一在提交失败时反向执行；`move_images` 至少复用 `place_many` 的 `done` 回滚模式。

#### L3. `cleanup_undo_dirs` 可在另一个写事务提交前删掉它的暂存目录

- 位置：`library/history.rs:1400-1427`、`library/edit.rs:220-225`、`app/state.rs:1530-1550`
- 事实：cleanup 用读连接枚举 `operations` 后按目录名删除；`edit.rs:220-225` 在 `history::write` 事务内部创建 `.artbank/undo/<id>/` 并硬链接旧原图，提交前该行对读连接不可见；`after_history_write` 每次写入后都会调用 cleanup。
- 影响：编辑保存进行中，另一条命令（保存设置、MCP 写入）触发 cleanup，会把未提交记录对应的暂存目录当“无主目录”删除。编辑随后提交成功，撤销时 `stage_file` 找不到旧原图，编辑永远不可撤销（旧原图已被删除）。
- 建议：`cleanup_undo_dirs` 在 `with_writer` 内执行（等正在提交的事务结束后再枚举+删除），或维护进程内“在建操作 id”集合并跳过；只删除 `operations` 中确实已删除的 id。

#### L4. `move_orphans` 在单写事务里搬完全部文件，崩溃丢掉整条记录，长 I/O 阻塞所有写

- 位置：`library/maintenance_actions.rs:89-154`、`:186-196`、`app/state.rs:2576-2579`
- 事实：先在事务里插入 `pending` 记录，同一事务内逐个 `move_file`，最后才写 chunk 并提交；跨卷时复制 + 删除源。
- 影响：进程在提交前被杀，SQLite 回滚整条 `operations` 行，但文件已经进了 `recycle/orphans/`，没有记录可撤销；清理几百 GB 时唯一写连接被长期占住，导入 flush、元数据编辑、MCP 写入全部排队。
- 建议：先在事务外完成搬移并落 staged 列表，再用短事务写记录与 affected；失败用反向搬移补偿；或按批提交多条 chunk（撤销已支持多 chunk）。

#### L5. 待释放状态机不闭合：失败被吞、非法路径行永远删不掉、非空目录让 `purge_operation_id` 永久为真

- 位置：`library/metadata.rs:1670-1676,1689-1719,1749-1808`、`library/history.rs:1484-1499,4628-4640`、`app/state.rs:1621`
- 事实：路径解析失败的行只 warn 并从 `files` 丢弃；先删行再删文件，删除失败只塞进 `report.failures`，`run_release` 丢弃 report；目录非空时保留 `purge_operation_id`，`has_orphan_purge` 永远为真，每次开库都再排一次释放任务。
- 影响：非法路径图片行不可见也永远不会被删；文件夹行无法释放；锁定文件失败无日志、无重试入口；每次启动重复一个无效释放任务。
- 建议：路径无效时仍删除行（失败清单列路径），或保留“待释放原因”；非空时清掉 `purge_operation_id` 并写可诊断记录，避免 `has_orphan_purge` 永久为真；`run_release` 至少把 failures 写日志/计入任务结果。

#### L6. 导入撤销对同一张图的多次合并按正序回放，最终恢复成中间态

- 位置：`import/pipeline.rs:1134-1152`、`library/history.rs:4410-4478`
- 事实：每次合并记录当时的 `old_score/old_modified_at/old_tag_ids`；`restore_import_merges` 按记录顺序正向执行。若 M1 把评分 3→5，M2 把 5→5 并加标签，撤销时先写 3 再写 5，最终停在 5。
- 影响：一次导入出现同一内容的多个副本（或跨批次重复命中）时，“撤销导入”无法恢复评分/标签/修改时间。
- 建议：按 `image_id` 分组只应用最早一条快照（或逆序迭代并跳过后续同 id 项）；补“同一图片在两条 `ImportRecord::Merged` 中”的测试。

#### L7. 撤销导入时原图缺失会让这次重做永远失败，阻塞整条重做链

- 位置：`library/history.rs:3959-3968,4308-4313`、`app/state.rs:1457-1468`
- 事实：找不到原图时记录 `staged_file: None`；`restore_import_file` 直接报“原图文件缺失”；没有单条跳过路径，`run_redo` 失败后整链停住。
- 影响：只要撤销导入时有一张原图被外部删掉，这条导入就永远无法重做；用户只能清空记录，而清空记录会释放其他待删文件。
- 建议：把缺失文件记为显式“缺文件”状态，重做时恢复行并允许该行缺失（与检查报告 missing 口径一致），或允许单条跳过并保留可重试标记。

#### L8. 恢复图片的“文件夹不在回收站”前置检查在事务外，且与文件夹删除使用不同资源锁

- 位置：`library/metadata.rs:1401-1425,1433`、`app/state.rs:1114-1119,1242-1252`
- 事实：用读连接检查 `folders.trashed_at`，随后才开写事务；文件夹删除走路径写，恢复图片走元数据写，两者不互斥。
- 影响：检查通过后文件夹刚好被整目录移入回收站，恢复的 UPDATE 会把图片 `deleted_at` 清空，形成“正常图片位于已删除文件夹”的非法状态；之后空回收站会因该文件夹仍有图片而无法释放。
- 建议：把检查移进 `set_trashed_with_id` 的写事务，或让恢复图片也申请文件夹路径读锁。

#### L9. 文件夹路径 SQL 不敏感、Rust 敏感，可绕过“移动到自身子树”检查并丢计数

- 位置：`library/import.rs:73-103`、`library/folders.rs:529-531,630-635,852`、`library/query.rs:1082-1101,2470-2496`
- 事实：SQL 用 `COLLATE NOCASE`，但新子目录按调用方大小写写入；`is_in_subtree` 与 `place_many` 用大小写敏感 `starts_with`，同文件其他位置又用 `eq_ignore_ascii_case`。
- 影响：`place_many` 的“目标父级不能位于被移动文件夹内部”检查对大小写不同的路径失效，可把文件夹移进自己的后代形成环；`expand_folder_counts`/`subfolder_count` 按精确前缀匹配，子路径大小写不一致时祖先计数与子文件夹数丢失。
- 建议：`ensure_folder_tracked` 返回数据库中已存的规范路径，后续子路径一律基于它拼接；祖先/子树判断改为基于 id 的递归 CTE，或统一用分段 `eq_ignore_ascii_case`；`expand_folder_counts` 改成按 id 自底向上累加。

#### L10. 缩略图/特征包只 `flush` 不 `fsync`，而压缩用 `synchronous=FULL` 提交指针

- 位置：`library/thumbs.rs:129-131,227-240`、`ai/pack.rs:149-157`、`library/optimize.rs:1016-1067`、`library/query.rs:806-808`
- 事实：append 只 flush；提交指针的事务是 durable；缺失判定只看 `thumb_segment/thumb_length` 列。
- 影响：掉电/强杀后数据库认为缩略图存在，实际段被截断，网格长期 404，只有全量“重建缩略图”能恢复；特征包同理，embedding 记录存在但读取失败，语义搜索静默少图。
- 建议：段轮换、提交前或每 N 次 append 做一次 `sync_all`；把“指针越界/字节缺失”纳入 `PendingFilter::Thumbnail`/特征缺失判定，或启动时校验段文件长度。

### 4.3 P2

| 问题 | 位置 | 影响 | 建议 |
| --- | --- | --- | --- |
| `undoFailed` 没有单条重试/解除入口 | `app/state.rs:1407-1429`、`history.rs:1464-1482` | 一次瞬态失败让整条时间线不可用；唯一出口“清空记录”会物理释放所有待删文件 | 提供“重试这条/跳过这条”；清空记录与释放文件拆成两个显式动作 |
| 编辑保存后不清有序 id 页缓存 | `app/commands.rs:953-959`、`app/state.rs:1530-1550` | 按修改时间/大小/多级排序时，`page_token` 仍命中写入前的 id 列表，编辑后翻页顺序错 | 在 `after_history_write` 统一 `query::clear_page_cache()` |
| 关键不变量只靠代码维护，schema 无 CHECK/触发器 | `library/schema.rs:16-27,36-70,181-207` | 漏一步就落成非法行，之后只能靠 WHERE 兜底 | 新库加 `CHECK (kind='normal' OR trashed_at IS NULL)` 等；老库迁移时重建表 |
| 迁移链是 9 个手写 match 臂 | `library/schema.rs:265-349` | 新增版本要改 9 处，漏一个就让某个旧版本无法升级 | 改 `const MIGRATIONS: &[(u32, fn)]` + 循环 |
| 生产代码仍保留 `FolderDelete` 分支 | `history.rs:44-45,3169-3172,3288-3301`、`folders.rs:1624-1666,1820-1864` | 当前无生产者，但**不能删**：第三轮计划明确保留旧空文件夹删除记录的解析与撤销能力（`docs/planning/archive/2026-10-29-使用反馈整改计划（第三轮）.md:240`） | 保留；可在注释里标明“仅旧记录兼容” |
| `metadata::ImageState` 与 `history::ImageState` 同名不同义 | `metadata.rs:43`、`history.rs:508` | 两个类型同时被 commands/state 导出，容易误用 | 改名 `ImageVisibility` |
| 编辑保存后的 AI 特征文案与行为不符 | `src/modules/viewer/ViewerTools.tsx:470`、`library/edit.rs:262-263` | 写“保存后重新生成”，实际 `DELETE FROM feature_blobs` 不重算 | 改为“AI 特征会被清除，需要重新在批量任务里生成” |

---

## 5. 前端状态与竞态

### 5.1 P1

#### F1. 保留旧行刷新按 `previousCount` 截断，页边界之后会永久漏一行

- 位置：`src/modules/library/useImageQuery.ts:123-188`、`LibraryView.tsx:613-615,690-691`
- 事实：`removeItems` 先本地删行，`refreshKeepingRows` 以 `previousCount = items.length` 为目标抓页，最后 `collected.slice(0, target)`，游标取最后一页末尾。原 150 行删 1 行后 target=149，抓 150 行只留 149 行，游标停在第 150 行之后，第 150 行（新的）永不显示。
- 影响：每次小批量删除都可能在页界留洞；用户滚到该位置会发现少一张，且刷新后才恢复。
- 建议：不要截断到 `target`——保留 `collected`（多显示少量行），或目标行数上取整到 `PAGE_SIZE`，或最后一页按剩余行数调整 `limit`；补游标连续性测试。

#### F2. 详情栏切图不先清空 `detail`，请求返回前显示上一张，写操作可能落到上一张

- 位置：`src/modules/library/Inspector.tsx:313-339,420-447,490-553`
- 事实：`useEffect` 只 `setLoading(true)`，不 `setDetail(null)`；异步完成后无条件 `setDetail(next)`。渲染标题用当前 `imageId`，内容仍是旧 `detail`；点星级、加删标签、改描述、移动文件夹都使用 `detail.id`。
- 影响：从 A 切到 B 后，请求返回前标题是 B、内容是 A；请求失败会一直显示 A。此时任何写操作都落到 A。移动目录尤其严重。
- 建议：`ImageInspector` 以 `imageId` 作 `key`，effect 开始时清空 detail；所有 `setDetail` 用 `previous.id === 操作时的 imageId` 守卫；抽 `useImageDetail(imageId, version)` 统一取消与 id 校验。

#### F3. 缩略图 URL 只有 `imageId + version`，切库后跨库串图

- 位置：`src/shared/thumbUrl.ts:33-41`、`src/shared/thumbCache.ts:16`、`src-tauri/src/app/thumb_protocol.rs:11,74`
- 事实：URL 是 `/{imageId}?v={version}`；缓存是模块级 LRU、无图库维度；协议返回 `max-age=31536000, immutable`；切库只重挂 `LibraryWorkspace`（`AppShell.tsx:76-80`），没有清缓存。
- 影响：两个库都存在 id=1、thumb_version=1 时 URL 完全相同，切库后网格/胶片带/相似组直接命中上一个库的缩略图，且长期不失效。
- 建议：URL 或缓存键加入图库实例标识（如 `?lib=<libraryId>`），切库时 `thumbCache` 以图库为命名空间；协议侧 ETag 无法区分库，只能靠 URL。

### 5.2 P2

| 问题 | 位置 | 影响 | 建议 |
| --- | --- | --- | --- |
| 查看器不跟随原图内容变化重新加载 | `ImageViewer.tsx:246-302`、`ViewerHost.tsx:61-82` | 空间优化替换当前图后仍显示旧内容；旧文件释放后变坏图，编辑基于过期 source | 把 `contentHash` 纳入加载 key，或 content 类变化时关闭查看器 |
| 图片离开当前筛选/集合后查看器仍停留 | `ViewerHost.tsx:24-29,61-82` | 查看器显示“不在当前列表”，←/→ 失效 | 刷新后检查 `session.getItems()` 是否仍含 activeId，不在则关闭 |
| 全局快捷键穿透模态框 | `AppShell.tsx:26-57`、`Modal.tsx:18-33,92-100` | 导入运行中可叠设置/图库对话框，与“完成前不能用其他功能”矛盾 | `Modal` 暴露 `isModalOpen()`，栈非空时忽略快捷键 |
| 相似结果下“新建集合”数量不一致 | `LibraryView.tsx:345-346,1281-1284,2713-2719`、`CollectionDialogs.tsx:73-108` | 文案写底层视图总数，实际写入相似结果 id | `viewCount` 改用 `similarity.total`，禁用条件同源 |
| 相似结果打开时点侧栏文件夹，结果不退出 | `LibraryWorkspace.tsx:629-657`、`LibraryView.tsx:1844-1862` | 网格仍是旧文件夹的相似结果，左侧高亮新文件夹；批量任务范围提示错误 | 文件夹选择统一走“离开相似结果”入口 |
| 前端没有镜像后端查询上限 | `library/query.rs:1643-1662`、`collections.rs:21`、`model.ts:178-203` | 257 个文件夹/65 个标签/201 字搜索/33 个集合运算会让画布变成“图片列表读取失败” | 在 `model.ts` 集中定义并裁剪/拒绝，控件显示上限 |
| 标签写入失效漏 `tagPresence`/`excludeTagIds` | `dataChange.ts:126-128,198-211,225-230,491-493` | “无标签/不含标签”筛选下加标签，行仍留在列表；移除 A 但仍有 B 时选择被误裁 | `tagsAffectQuery` 纳入这两项；按写入后的已知标签集重算选择 |
| 文件夹移动后选择不裁剪 | `FolderTree.tsx:465-479`、`LibraryWorkspace.tsx:603-627`、`dataChange.ts:232-249` | 按文件夹筛选时把选中图片移走，刷新后图片消失但选择仍包含它们 | `moveImagesTo` 携带 ids 与 targetFolderPath；无法枚举时清空选择 |
| 扩展名分面吞掉已选但计数为 0 的项 | `FilterPanel.tsx:110-112,229-242` | 选了 gif 再叠加筛选后 chip 消失，无法单独取消 | 取“分面值 ∪ 已选值” |
| 删除/合并被筛选标签后筛选条件不清理 | `dataChange.ts:287-301`、`LibraryView.tsx:631-639` | 筛选条仍带不存在的标签 id，列表为空且无法取消 | `tagCatalog` 计划增加“清掉已删除/合并源标签”动作 |
| 维护/相似分组范围上下文不刷新 | `LibraryWorkspace.tsx:537-577,821-826,844-849` | 对话框打开期间数据变化，仍显示旧张数/旧范围 | 四类上下文统一进 `contextsRevision` 或抽 `TaskScopeContext` |
| 关闭的对话框仍订阅整个任务列表 | `LibraryWorkspace.tsx:813-875`、`OptimizeDialog.tsx:347,503-534` 等 | 任一大任务运行时四个关闭对话框仍逐次重渲染 | 按 `open` 条件挂载，或加 `useTaskById` 窄订阅 |
| 导入运行中不可关闭且无兜底出口 | `ImportDialog.tsx:360-363,412` | 任务丢失/后端卡住时模态框永久阻塞，只能关窗口 | 停滞超时 + “转到后台/强制关闭”；取消进入“取消中”并防重复 |
| 设置草稿被后台 settings 变化重置 | `SettingsDialog.tsx:151-166` | 打开期间侧栏分栏/详情栏宽等静默保存会覆盖未保存草稿 | 只在 open false→true 时克隆；对 settings 更新做 dirty 判断 |
| 任务中心初始列表可能覆盖事件 | `TaskCenterProvider.tsx:57-68` | 启动瞬间结束的任务永远显示“进行中” | 先注册监听再 list，按 taskId/startedAt 合并 |
| `statsStore.fetch` 失败可能把旧值当新值返回 | `statsStore.ts:217-230,299-302,348-352` | 批量/优化对话框拿到过期计数且无错误提示；淘汰当前项后不自动重取 | 无新数据且 request 出错时抛错；get 命中 touch，或 useStats 补 ensure |
| 39 万全选制造多份大数组并全量发 IPC | `LibraryView.tsx:1057-1059,1302-1317,2009`、`useSelectionTags.ts:48-63` | 全选后复制/批量侧栏明显卡顿 | 复用同一份 selectedIds；批量标签候选延迟到展开时查；必要时分批 |
| 筛选面板标签固定只取前 500 个 | `library/api.ts:174-182`、`FilterPanel.tsx:70-86,407-412` | 标签库超过 500 时靠后的标签既看不到也搜不到 | 有输入时走后端搜索，或显示截断提示 |
| FolderTree 写操作基于渲染快照，移动图片双重整树刷新 | `FolderTree.tsx:342-352,455-471` | 快速连续重命名/移动可能后一个覆盖前一个；移动图片读两次树 | 改成 updater/统一 `FolderChange` 合并；串行化文件夹写；移动图片只发 `DataChange` |
| 图库名称自动填充不跟随路径更新 | `LibraryDialog.tsx:56-60` | 目录 A 改为 B 后名称仍是 A | 记录“是否自动填充”，路径变化时同步刷新，用户手改后停止 |

### 5.3 P3

- `LibraryView` 的 `contextRevision`（`LibraryView.tsx:296`）从未使用。`model.ts:16,18` 的 `DEFAULT_SORT/COLLECTION_SORT` **不是死代码**：它们在 `:17,19` 被用来构造 `DEFAULT_SORT_LEVELS/COLLECTION_SORT_LEVELS`，不要删。
- `dataChange.ts:617-633` 的按 MCP 工具名回退分支，当前所有 MCP 写工具都返回结构化 `change`，属于旧版兼容；若保留需注明最低兼容版本。
- `mock.ts:298-327` 忽略 `folderDirect/tagPresence/excludeTagIds`，`embedding/pose=has` 恒 false；`mockConfig.ts` 仍被引用，**不是死代码**，但 mock 语义需要补齐或标注。
- `index.css` 约 200 行死规则：`.ai-dialog/.ai-kind-grid/.ai-job/.ai-progress/.ai-failures`（`:5174-5365`）、`.batch-task-conflict`（`:6148-6175`）在 `src` 中无引用。
- `index.css:33` 注释“Modal 用计数维护”已过期（实际是模块级栈）；分区名仍是 `S3`；3 处字面色值（`:1326-1327,5097`）违反 tokens 约定。
- `settings.rs:77` 注释仍称“批次 1 尚未注册写工具，实际能力为只读”，实际已注册 24 个工具。
- `commands.rs:229-246` 重复定义 `LOG_FILE_STEM`，注释指向已迁走的 `lib.rs`。
- `maintenanceApi.ts:8-11` 注释写“包回收都走 start_batch_task”，与实现相反。

---

## 6. 死代码、过期注释与补丁式功能

### 6.1 可直接删除/收敛

| 项目 | 证据 | 处理 |
| --- | --- | --- |
| `BatchTaskKind::PackCompact` 分支 | `state.rs:1876`、`batch_preview.rs:98-106`；前端只用专用命令 | 删枚举分支与预览拒绝分支，或统一入口 |
| `TaskLane`/`lane` | `tasks.rs:1-8,141-147,333-335,536`；前端未消费 | 删字段与注释，或真的用于任务中心分组 |
| `ImportRequest.worker_threads` | `import/mod.rs:85-86`；永远 `None` | 删字段与测试构造 |
| `undo_runs` | `schema.rs:201-207`；只写不读，不清理 | 删除，或真正做数据库级互斥并级联清理 |
| `FilterCounts.folders` | `types.ts:113`；`FilterPanel` 不消费 | 删 DTO、两处 SQL 分支与 mock，同步文档 |
| `LibraryView.contextRevision` | `LibraryView.tsx:296` | 删除 |
| ~~`model.ts` 的 `DEFAULT_SORT/COLLECTION_SORT`~~ | `model.ts:16-19` | 复核后撤回：内部构造 `*_SORT_LEVELS`，不是死代码 |
| `thumbs.rs` 的 `#[allow(dead_code)]` 字段 | `thumbs.rs:37,39` | 确认是否调试需要，否则删除 |
| `deletion_blocker` 未用参数 `root` | `folders.rs:1315-1327` | 去掉参数 |
| 旧 CSS 规则 | `index.css:5174-5365,6148-6175` | 删除 |

### 6.2 应保留（避免误删）

| 项目 | 原因 |
| --- | --- |
| `HistoryKind::FolderDelete` 及其 undo/redo | 第三轮计划明确保留旧空文件夹删除记录的解析与撤销能力，不做数据迁移（`docs/planning/archive/2026-10-29-使用反馈整改计划（第三轮）.md:240`）。当前无新生产者，但有旧记录消费者 |
| `mockConfig.ts` | 仍被 `mock.ts:1` 引用，浏览器预览需要 |
| `platform/fs.rs` 的 `rename_durable` | 导入/编辑/压缩发布顺序的关键，不是重复实现 |
| `undo_runs` 的恢复 UPDATE | 若保留表，则 `recover_on_open` 对 running 的兜底是必要的；若删表，需连同恢复逻辑一起改 |

### 6.3 过期/误导注释

| 位置 | 现状 | 建议 |
| --- | --- | --- |
| `app/settings.rs:77` | 称 MCP 只读、批次 1 未注册写工具 | 改为“默认关闭、默认询问档；工具与三档语义见 mcp.md” |
| `app/commands.rs:229-246` | `LOG_FILE_STEM` 重复，注释指向 `lib.rs` | 从 `app/logging.rs` 导出常量给 commands 用 |
| `src/app/maintenanceApi.ts:8-11` | 称包回收走 `start_batch_task` | 改为专用 `start_pack_compact` |
| `src/styles/index.css:33` | 称 Modal 用计数维护 | 改为“模块级栈” |
| `src/styles/index.css` 分区名 | `S3 views/dialogs/filter additions` | 改为功能名 |
| `src/modules/viewer/ViewerTools.tsx:470` | 称 AI 特征保存后重新生成 | 改为“会被清除，需要重新生成” |
| `src/app/SettingsDialog.tsx:510-513` 等 | 一个开关三段小字，解释“段文件里留下空洞” | 按 AGENTS.md“界面文案”收敛为一句后果描述 |
| `src/app/MaintenanceDialog.tsx:368` | “范围与批量任务共用同一套口径” | 删，属于实现说明 |
| `src/app/McpPanel.tsx:149` | “AI 客户端通过同一份图库状态读写” | 改为用户关心的“只接受本机 127.0.0.1 连接” |

---

## 7. 文档与代码一致性

### 7.1 已核对一致的数字

| 项目 | 文档值 | 代码值 | 结论 |
| --- | --- | --- | --- |
| 设置版本 | 7（`library.md`、`current-handoff.md`） | `app/settings.rs:12` = 7 | 一致 |
| schema 版本 | 10（`library.md`、`current-handoff.md`） | `library/schema.rs:8` = 10 | 一致 |
| MCP 工具数 | 24（`mcp.md`、README、docs/README） | `mcp/server.rs:40` = 24 | 一致，但四处重复维护 |
| 命令记录 | 100 命令 / 92 包装 / 写 58 / 读 34 / 无 Result 8 | `commands.rs` 注册 100；`trace::write` 58、`trace::read` 34；未包装 8 | 一致 |
| 排序指标 | 7 个、最多 7 级 | `settings.rs:21-29`、`query.rs:29` | 一致 |
| 统计缓存 | 48 | `statsStore.ts:47` | 一致 |
| 日志轮换 | 4MB × 5 | `app/logging.rs:12,14` = 4,000,000 字节、KeepSome(5) | 一致（十进制 MB） |
| 默认线程 | 逻辑核数 − 1 | `concurrency/budget.rs:117-121` | 一致 |
| pHash 阈值 | 6/256 | `library/similarity.rs:17` = 6 | 一致 |
| embedding 阈值 | 0.97 | `ai/runtime.rs:1082` | 一致 |
| 像素上限 | 编辑 5000 万 / 预览 2400 万 | `imageops/edit.rs:13`、`library/viewer.rs:21` | 一致 |
| 解码上限 | 单边 65535、1 亿像素、512 MiB | `import/decode.rs:8-10` | 一致 |

### 7.2 需要修正的文档问题

| 级别 | 问题 | 证据 | 建议 |
| --- | --- | --- | --- |
| P1 | 缩略图段数与体积都是旧测量 | `current-handoff.md:28` 写 10 段，`:31` 写“9 段 9.24 GB”；`todo.md:101`、`maintenance.md:56` 也写 9 段；2026-10-29 只读清点真实目录为 `0001`–`0010` 共 10 段、约 9.89 GB（9.2 GiB） | 段数改 10，体积按同一测量更新；或基线只写“按磁盘实际”，不写具体段数。三处一起改 |
| P1 | 维护文档仍描述已被删除的“多图库/移除图库” | `maintenance.md:145-149` 与 `library.md:344`、`overview.md:43` 冲突 | 删“图库管理”小节或改为“单库：打开/关闭/换库见 library.md，无移除操作” |
| P1 | 设置字段契约漏 `maintenance` 组 | `library.md:121-133` 无 `maintenance`；`settings.rs:47,61,334-346` 有 `MaintenanceSettings { auto_compact_thumbs }`，默认 true，缺键不升版本 | 在 library.md 字段块补 `maintenance { autoCompactThumbs }`，写明默认值与缺键语义 |
| P2 | 标签分面口径容易被读成互相矛盾 | `browse-and-query.md:81` 的“两种情况都不做过滤”只适用于“无标签”模式，`:84` 是一般规则；代码 `FilterPanel.tsx:127-139` 与两者一致 | 无需改行为；把 `:81` 的“两种情况”写明为“‘无标签’模式下”，避免误读 |
| P1 | AI 阅读引导仍把“描述导入导出”当现有路由 | `ai-reading-guide.md:16`；`maintenance.md:7-11` 明确“描述导出/回填不做” | 从路由删掉“描述导入导出” |
| P2 | README 写“上百万张”，架构写“几十万张”，基线 396,180 | `README.md:3`、`overview.md:3`、`current-handoff.md:28` | README 只作定性介绍，数字只在 current-handoff |
| P2 | 性能数字散落在模块/架构文档，已有过期值 | `overview.md:140-141`、`library.md:42,46`、`ai-service.md:101,228,316`；`ai-service.md:242` 仍写“真实库 364,056 行”，基线是 396,181 | 数字统一回 `guides/performance.md`，模块文档只留方法与原因 |
| P2 | 模块文档各自维护“尚未实现”，绕过 todo 唯一清单 | `browse-and-query.md:434-439`（含 todo 中没有的“相似度作为筛选/排序”）、`import.md:174-179`、`viewer-and-editor.md:90-95` | 把缺的那条补进 todo，其余改为链到 todo |
| P2 | `library.md:355-379` 的测试清单与 `validation.md:54-60` 重复 | 同一批覆盖写两处 | 模块文档删“## 测试”，保留一句指向 validation.md |
| P2 | 命令契约表漏 `start_pack_compact`；`set_import_optimize` 有正文无表项 | `commands.rs:1263-1270,75-81`；`docs/` 中 `start_pack_compact` 零命中；`import.md:55` 已描述 `set_import_optimize`，但 `:145` 的命令表没有 | 在 maintenance.md 补 `get_pack_stats/start_pack_compact`；在 import.md 命令表补一行 `set_import_optimize` |
| P2 | `docs/README.md` 的“规范”只列 validation/performance，漏 logging | `docs/README.md:28-31,33-40`；`logging.md` 实际存在且被多处引用 | 加一行“日志与命令记录” |
| P2 | 描述能力的禁用口径与代码不一致 | `ai-service.md:189,319` 称批量置灰；`browse-and-query.md:398` 写完整成功流程；代码只在批量侧置灰（`BatchTaskDialog.tsx:125`），详情按钮只判 `aiBusy/aiBlocked`（`Inspector.tsx:897-908`） | 明确“仅批量置灰；单张在服务返回 404 时报错”，或也按能力禁用按钮 |
| P2 | `browse-and-query.md` 439 行、15 个二级标题，契约/实现/UI/性能/待办混杂 | `browse-and-query.md` | 把前端文件清单与交互移出，性能与待办只留链接 |
| P2 | 架构分层漏平台文件系统能力 | `overview.md:29` 只写剪贴板；`platform/fs.rs` 提供 `rename_durable` 与目录 flush | 补“文件系统（原子 rename、目录 flush）” |
| 已修 | ~~待办里的 schema 目标版本过期~~ | B3 已把 `todo.md:20` 改为“当前 schema”并补 v9→v10 | 无需再改 |
| P3 | 两处引用需要写准 | `mcp.md:71` 的“当前没有导出工具”在 MCP 上下文里成立，但容易被读成全局；`ai-service.md:258` 引号内的“需要重算 embedding”不是现有界面原文 | 改为“MCP 不提供导出工具”；AI 文案改成代码里的真实提示或去掉引号 |
| 已修 | ~~`logging.md` 仍指向 `lib.rs`~~ | B3 已改为 `app/logging.rs`，并补了无头目标说明 | 无需再改 |
| P3 | `maintenance.md:97` 指向不存在的 `src/app/taskScope.ts` | 实际在 `src/shared/scope.ts` | 改指针 |

### 7.3 文档结构判断

- `docs/modules/*.md` 总体质量高，契约、命令、边界都有；主要问题是**同一事实多处维护**与**“尚未实现”散落**。
- `docs/planning/current-handoff.md` 是有效的“现状单一入口”，但第 28/31 行已经出现同一文档内的数字漂移，说明“一个数字只维护一处”的规则需要更严格执行。
- `docs/planning/archive/` 的索引基本可用；本轮“无窗口验收计划”从 archive 移回 planning 的改动是进行中的工作，不在本报告建议范围内。
- 前端界面文案确有“解释实现而非影响决策”的段落（见 6.3），建议在下一轮 UI 打磨时按 AGENTS.md 的“界面文案”规则统一清理。

---

## 8. 建议整改顺序（按 `c80f6cf` 重排）

> 前提：B1–B3 已完成并提交，本报告不再要求改动它们。批次按“实现 + 测试”估算，机械搬迁与纯文档不计入 2–3k 行；具体决策项在对话中逐项确认，确认后回填本报告与 `todo.md`。

| 批次 | 内容 | 估算新增 | 依赖 |
| --- | --- | --- | --- |
| A 数据安全 I | L1、T1、L3、L9 | 1.8–2.2k | 先定 L1 失败语义；L2 原语可在此设计 |
| B 数据安全 II | L2、L4、L10 | 1.5–2.0k | 依赖 A 的补偿方向；L9 先行 |
| C 释放与导入历史 | L5、L6、L7、L8 | 1.3–1.8k | L5 在 T4 前 |
| D 任务/关库/终态 | T2、T3、T4、T5、T10、T11、T12、T13 | 2.3–2.8k | 先定终态；T13 在 T2 后 |
| E AI/MCP 生命周期 | T6、T7、T8、T9（+ Python 最小加固） | 1.6–2.1k | 独立 |
| F 前端身份与失效 | F1、F2、F3 + 6 个高影响 5.2 | 2.0–2.5k | 独立；F3 用现有 `LibraryInfo.id` |
| G 内存预算 | P1 第 10 项 | 1.0–1.5k | 依赖 D/E 的取消语义 |
| H 清理/文档 | 6.1、6.3、7.2 | 0.5–1.0k | 依赖 A–F 的最终行为 |
| I 结构去重 | 文件夹聚合/ID/路径/任务通道/WriteGate/进度 UI | 2.0–2.7k | 在拆分前；需快照测试 |
| J 拆分 | 巨型文件、CSS、`shared/` 下沉 | 0（机械，约 8–12k 行搬迁） | 最后，一次一个文件/提交 |

### 8.1 体量判断

- A/B/C 的行数在 1.3–2.2k，接近目标；A 是数据安全，宁可小于上限也不要塞满。
- 初版第 B 批跨任务总线、AI、MCP、进度四个子系统，行数可能刚好 2.5–3k，但评审和回滚面太大；本版拆成下表的 C（释放/历史）、D（任务/关库）、E（AI/MCP）三块，每块 1.3–2.8k。
- 初版第 D 批以删除和文档为主，新增代码不到 1k；不按 2–3k 要求，单独作为清理批次。
- 初版第 E 批是机械搬迁，不计入行数，但一个批次搬 8–12k 行太大；按文件拆成多个提交，每次只搬一个文件并跑对应测试。
- 初版没有给 P1 第 10 项、L6–L10、Python 与结构重复留位置，本版补成 G/E/I。

### 8.2 依赖

- `L2` 的补偿原语先于 `L1/L4`，否则会重做一次；`L9` 的路径规范化先于文件补偿。
- `L5 → T4`；`T2 → T13`；`T5/T10/T11` 先定终态再写三处收尾。
- `G` 在 `D/E` 之后，避免预算等待变成新的不可取消点。
- `H` 在 `A–F` 之后；`I` 在 `H` 之后；`J` 最后。
- `F` 与后端批次基本独立，可以并行。

### 8.3 验收口径

- 每个批次按 `docs/guides/validation.md` 跑对应检查；涉及写路径、任务、维护、AI、MCP 的批次，提交前加跑相关的 `cargo test --features headless --test acceptance_*`。
- 失败注入测试优先加进 `headless` 场景或现有 `acceptance_*`，不要另建临时测试框架。
- 每个批次单独提交，不把拆分与行为修复混在一起。

### 8.4 不建议现在做

- 不要为了“整洁”删除 `FolderDelete` 旧记录分支——真实库可能仍有旧 `folderDelete` 记录。
- 不要在行为修复前重写 schema 或迁移链；CHECK 需要新迁移，放在独立批次。
- 不要先拆巨型文件再做结构去重；否则重复实现会被搬进更多文件。
- 不要把 `mock.ts` 当死代码删除；浏览器预览仍依赖它。

---

## 9. 审查验证状态

### 9.1 主审直接复核（✅）

- 多文件夹 undo/redo 单事务与磁盘 rename：`history.rs:3110-3319`、`folders.rs:1445-1497`
- 导入写线程 panic 脱离：`pipeline.rs:256-330`、`import/mod.rs:195-211`
- 关库与空闲回收竞态：`state.rs:858-899`、`idle.rs:121-142`、`tasks.rs:1055-1085`
- 撤销暂存目录清理竞态：`history.rs:1400-1427`、`state.rs:1530-1550`
- 孤儿清理长事务：`maintenance_actions.rs:89-154`
- 导入合并撤销正序回放：`history.rs:4410-4478`
- 缩略图连续失败 `Completed`：`state.rs:1935-1972`
- AI 监督标志：`runtime.rs:1356-1368`（并对照 `:1470-1479` 的 guard 逻辑）
- 刷新保留行截断：`useImageQuery.ts:123-188`
- 详情栏切图竞态：`Inspector.tsx:313-339,420-447`
- 缩略图跨库缓存：`thumbUrl.ts:33-41`、`thumbCache.ts:16`
- 文档：`current-handoff.md:28/31`、`todo.md:101`、`maintenance.md:145-149`、`library.md:121-133`、`browse-and-query.md:81/84`、`ai-reading-guide.md:16`
- 死代码：`TaskLane`、`worker_threads`、`FilterCounts.folders`、`contextRevision`、`undo_runs`、`PackCompact` 前端入口
- `FolderDelete` 旧记录兼容保留：`docs/planning/archive/2026-10-29-使用反馈整改计划（第三轮）.md:240`

### 9.2 子代理定位、主审按代码结构判断（◐）

- 优化/估算/查看器/AI/MCP 未走字节预算：`optimize.rs:675`、`viewer.rs:117,184`、`mcp/sheet.rs:269,286`、`ai/input.rs:87-91`
- 缩略图/特征包只 flush 不 fsync：`thumbs.rs:129-131,227-240`、`ai/pack.rs:149-157`
- 恢复图片与文件夹删除锁不一致：`metadata.rs:1401-1425,1433`
- 路径大小写分裂：`import.rs:73-103`、`folders.rs:529-531,630-635`
- MCP 确认队列与连接生命周期：`approval.rs:133-140,266-273`、`mcp/runtime.rs:210-225,286-323`
- 前端相似上下文、筛选上限、标签失效、选择裁剪等：`LibraryWorkspace.tsx`、`dataChange.ts`、`FilterPanel.tsx`

### 9.3 未验证（○）

- 本报告没有重新运行任何构建、测试、基准或应用；B3 的自动检查结果见 `docs/planning/current-handoff.md`。
- 未在 39 万行真实库上复测性能与内存。
- 未做真实窗口/WebView2 行为验证；所有 UI 结论基于代码路径与状态机推演。
- Python 服务未启动，`/v1` 异常路径结论来自代码审读。

### 9.4 本版复核修正

- **撤回**“`DEFAULT_SORT/COLLECTION_SORT` 是死代码”：它们是 `*_SORT_LEVELS` 的构造值，删了会编译失败。
- **撤回**“`browse-and-query.md:82/85` 说法相反”：`:81` 只针对“无标签”模式，代码与两处口径一致。
- **撤回**“`set_import_optimize` 命令表漏项”：正文已描述，只缺一行表项。
- **标记已修**：`logging.md` 指向 `app/logging.rs`；`todo.md` 的 schema 目标改为“当前 schema”。
- **修正数字**：真实库缩略图是 10 段、约 9.89 GB；“9 段 9.24 GB”是旧测量。
- **补充覆盖缺口**：P1 第 10 项、L6–L10、Python 服务、结构重复已进入第 8 节批次。

---

## 10. 一句话总结

**这套代码的架构方向是对的，主要债务不是“设计错了”，而是“边界没闭合”**：磁盘与事务、任务与取消、请求与身份、旧缓存与新图库之间缺少最后一层守卫。先按第 8 节的 A–C 修数据安全与释放闭合，再修任务、AI/MCP 与前端竞态，随后清死代码/文档并做结构去重，最后拆巨型文件；不要反过来先做大重构。

---

## 11. 决策记录（2026-10-29）

| # | 事项 | 已确认的做法 |
| --- | --- | --- |
| 1 | 本轮范围 | P0/P1 + 6 个高影响 P2；其余进待办 |
| 2 | L1 多文件夹撤销失败 | 整次当作没发生：任一项失败就逆序把已改的目录改回去 |
| 3 | L2 文件与数据库不一致 | 统一文件补偿：记录本次改过的文件，提交失败时倒序回滚 |
| 4 | L4 孤儿清理 | 先把文件搬进回收站，再用短事务写记录；接受“崩溃后文件已在回收站但无记录”的窗口 |
| 5 | L5 坏路径行 | 保留行并标记；清掉待释放标记，把原路径写进释放报告和日志，不改 schema |
| 6 | L5 非空回收站目录 | 保留待释放标记；不在每次开库时自动重试，下次清空回收站时再试 |
| 7 | L6 导入撤销 | 恢复到导入前：同一图片多次合并只应用最早一份快照 |
| 8 | L7 重做缺原图 | 允许重做，把这张标成缺图；其他图片继续，不阻塞整条重做链 |
| 9 | L10 掉电一致性 | 换段/提交指针时 `sync_all`，启动时校验段长度与指针；不每次追加都同步 |
| 10 | 任务终态 | 新增“部分失败”，显示失败张数与未删除旧文件数 |
| 11 | T3 排队任务 | 排队时不占写库；只有真正开始写才申请资源 |
| 12 | T4 释放任务关库 | 允许关库：停在块边界，保留待删标记，下次释放继续 |
| 13 | T7 AI 超时 | 单次 120 秒、最多 3 次、总预算 5 分钟；取消能立刻打断请求与重试等待 |
| 14 | T8 MCP 执行卡住 | 不自动判失败；10 分钟无进度标“疑似卡住”，返回当前进度，AI 自己决定继续等或取消 |
| 15 | T9 关闭 MCP | 立刻断开已连接客户端，并让所有工具入口检查服务已关闭 |
| 16 | T12 同步命令改 async | 本轮不改，记入待办 |
| 17 | T13 跨库任务 | 切库重启后自然清空，不单独修 |
| 18 | Python 服务 | 本轮不修，记入待办 |
| 19 | F1 刷新补页 | 最后一页只请求还缺的几张，精确补到原行数 |
| 20 | F3 缩略图跨库 | 切库改为重启应用，重启前弹框确认；缩略图地址带图库 id |
| 21 | 标签 500 上限 | 筛选面板输入时走后端 `search_tags`，初始列表仍可只取前 500 |
| 22 | 撤销失败 | 本轮做单条重试/跳过；“清空记录”与“释放待删文件”拆成两个显式动作 |
| 23 | 死代码与技术项 | 由代理自行决定：删 `TaskLane`、`FilterCounts.folders`、`ImportRequest.worker_threads`、`contextRevision`、`BatchTaskKind::PackCompact` 分支；`undo_runs` 保留但加清理；`DEFAULT_SORT/COLLECTION_SORT` 保留 |
| 24 | 下一轮 | 结构去重、巨型文件拆分、schema CHECK/迁移链、T12、Python 加固、剩余 5.2 与 mock 语义 |

执行项已拆进[待办](../todo.md)的“整改批次（2026-10-29 全量审查）”；本报告保留为审查记录，本轮收口后归档。

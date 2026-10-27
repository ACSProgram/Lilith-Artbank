# 当前阶段状态

日期：2026-10-27

本文件只写当前生效的能力、基线与约束。逐批改动、与计划的差异、实测证据都在 `archive/`（索引见[规划归档](archive/README.md)）；待验收与未完成事项只写在[待办与待决策](todo.md)，不在这里重复。

## 当前状态

已排期的批次全部实现并归档，没有进行中的批次。

- **数据变化与刷新**：所有写入统一成 `DataChange`，由 `planDataChange` 决定列表、选择、文件夹、集合、图库计数、特征、分面、标签选项、详情与相似结果各自失效；排序与数据刷新保留旧行、重取已加载范围并按活动图锚定滚动，只有换结果集才清空与回顶。外部任务与 MCP 的范围变化清空选择，相似参考、比较图与查看器活动图主动复核。契约见[浏览与检索](../modules/browse-and-query.md)。
- **详情与批量编辑**：详情顺序为评分、标签、主色、文件夹、描述、文件；描述只读/编辑两态，草稿按图暂存、取消即丢弃，AI 生成直接保存并退出编辑；主色按权重比例显示、0 权重隐藏、点击复制十六进制；批量与导入共用 chip 标签输入，批量移除区分共同标签与部分标签（只读命令 `get_selection_tags`）。见[详情与批量编辑](../modules/browse-and-query.md#详情与批量编辑)。
- **文件夹树与会话状态**：排序改为右键一次性动作（`apply_folder_order`，一条 `FolderReorder` 记录，可递归）；树内有“直属图片 N”虚拟行与 `filters.folderDirect` 直属筛选；多选文件夹可一次拖动（`place_folders` + 排序位回滚 chunk）；窗口位置/大小/最大化由 `tauri-plugin-window-state` 持久化并在启动后做离屏回退；展开集合按图库存 `localStorage`。见[文件夹](../modules/library.md#文件夹)。
- **后台任务与并发**：长任务冲突时自动排队（同 lane FIFO、`write` 优先、`ai`/`scan` 并行），短写与撤销/重做/清空记录仍快速失败；任务线程 panic、线程起不来或提前返回都落到失败终态并释放 lane；长任务操作记录由 `LiveOperation`、撤销/重做由 `HistoryRunGuard` 兜底收尾；任务历史只裁剪已结束任务；记录面板显示记录数与撤销数据占用。
- **MCP 图库控制**：应用内 stateless Streamable HTTP 服务（只绑 `127.0.0.1` + 随机 Bearer 口令 + 程序目录的 `mcp-runtime.json`）与同 exe 的 `--mcp-stdio` 小桥；24 个工具覆盖浏览、搜索、拼图、写入、撤销/重做、任务、导入、临时区特征、文件夹与标签管理；标签与文件夹列表用 offset 游标，`tags_list` 返回 `total`；筛选有 `tagPresence` / `excludeTagIds`；`library_info.mcp` 用 `writeAllowed` + 结构化 `approval`，阈值只在询问档给出；三档权限（只读 / 询问 / 完全）与 10 分钟确认队列；`logs/mcp.log` 审计。永久删除、清空回收站、图片空间优化、像素编辑与普通图库的批量 AI 生成仍不开放。契约见 [MCP 图库控制](../modules/mcp.md)。
- **图片空间优化**：分层估算 + 两阶段界面、不可撤销的压缩记录、导入即压缩。估算按「有损源 / 无损源 × 6 个 bpp 桶」分层，跨层按候选体积分配样本、层内按体积等分切片，节省率与跳过分类按切片权重外推；误差区间以切片重抽（覆盖 100% 时为 0），节省空间与耗时各给一个；执行前复核三道关（参数指纹与张数/体积、候选集合指纹、逐张 `stat` 与目录存在性，缺失或变化超 1% 拒绝），执行中原图缺失单独计数、不触发“连续失败即停”。断电注入测试未做，见[待办](todo.md)。见[图片空间优化](../modules/maintenance.md#图片空间优化)与[性能基准](../guides/performance.md)。
- **AI 服务**：本机 HTTP `/v1` 服务的托管与便携分发；启动后台预热依赖导入（`ai.prewarmImports`），`ensure` 末尾做一次 warmup forward；均值在开库时后台算，`Computing` 时等待而不重算。描述本轮不实现。见 [AI 服务](../modules/ai-service.md)。
- **图库操作撤销**：schema v5 的操作记录、线性撤销与重做、延迟释放、`.artbank/undo/` 暂存；评分/描述/标签/集合/相似分组、导入、编辑、移动、文件夹全部接入。见[图库与元数据](../modules/library.md#操作记录与撤销)。
- **临时文件夹**：schema v7 的 `folders.kind = 'temp'`，与回收站同级的会话级暂存区；只能经导入进入，打开、关闭、换库与退出时清空，元数据改动不写操作记录。见[图库与元数据](../modules/library.md#临时文件夹)与[导入与缩略图](../modules/import.md)。
- 其余已完成阶段（S0–S7、单库与多级文件夹、旧库迁移、体验整改 P0–P4、使用反馈整改批次 1–7）的结论都在各模块文档里，记录见[规划归档](archive/README.md)。

## 基线

- 图库：主库 `G:\Lilith Image Library`（390 542 张、445 个普通文件夹 + 1 个临时文件夹、556 个标签（175 个有图）、81 649 条 pose、9 个缩略图段），独立测试库 `G:\Lilith Test Library`（9 455 张）；失败内容在 `G:\Lilith Image Library Failed`（247 条超过 1 亿像素）。
- 压缩候选：主库 274 068 张 / 467.44 GB / 1 939.08 GP；默认 200 张档位只覆盖候选体积的 0.078%，`s5`（有损源 bpp ≥ 0.50）一层占候选体积 82.8%。
- 存储版本：schema v8（v1→…→v8 链式迁移，v7 新增 `folders.kind` 与唯一的临时文件夹，v8 只加覆盖索引 `idx_images_visible_facets`）、设置 v7（v1→…→v7，v6 新增 `mcp`，v7 新增 `ai.prewarmImports`）。设置、主日志与 `ai-server.log` 都在程序目录，不读旧 AppData 设置；MCP 运行时文件是程序目录的 `mcp-runtime.json`（本地凭据，已 gitignore）。
- 自动检查（本阶段收尾）：`cargo fmt --check`、`cargo clippy --all-targets`（0 警告）、`cargo test --lib`（372 通过、9 ignored）、`npx tsc --noEmit`、`npm run test:web`（54 通过）、`npm run build`、`git diff --check`。
- 20 万行元数据基准（`perf_metadata_200k`）：单级排序首页 0.66-0.76 ms（文件夹 42 ms，走 id 列表）、6 级排序首次建 id 列表 213 ms、后续翻页 0.83 ms、offset 199 850 的深页 0.83 ms；选中集标签查询（`selection_tags`）在 2 / 1 千 / 2 万 / 20 万选中下 p50 为 0.21 / 3.13 / 56.13 / 204.95 ms；直属文件夹页 150 张 p50 0.16 ms、直属总数 0.04 ms。全部数字见 [性能基准](../guides/performance.md)。
- 压缩估算抽样离散度（`perf_estimate_sampling_spread`，232 张合成库、50 张档位 10 次）：节省空间 −0.1%…+0.1%，耗时 −9.1%…+9.1%（受 1 秒取整限制）。同一基准在机器有负载时耗时偏差会放大到 +94%，节省空间仍稳定——耗时外推完全继承采样那一刻的墙钟，详见[性能基准](../guides/performance.md)。
- 排序指标 7 个（导入时间 / 修改时间 / 评分 / 文件大小 / 像素尺寸 / 文件夹 / 集合内顺序），最多 7 级；单级且非文件夹走键集游标，其余走进程内有序 id 列表。

## 约束

- `G:\Lilith Image Library` 已由迁移器改造为 Artbank 库；旧 `.data` 已删除，不要再让旧程序写入该目录。
- 应用不读旧 `.data`；迁移器在仓库外 `research/library-migration`，不进入 `src-tauri` 与产品提交历史。
- 真实库只用于验收；测试、基准和演示仍使用临时目录或复制样本，避免写入真实库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏。

## 归档

历史计划、批次记录与实测证据都在 [规划归档](archive/README.md)，那里是唯一的归档索引；本文件不再维护第二份索引。

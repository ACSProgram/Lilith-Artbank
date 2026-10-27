# Lilith Artbank 文档

文档只写两类内容：**当前有效的契约**（架构、模块、规范）与**还没做完的事**（待办、路线）。历史批次、被替代的计划与实现记录都在 `planning/archive/`，不作为执行依据。

## 按目标进入

| 你的问题 | 去哪 |
| --- | --- |
| 这个软件能做什么 | [README](../README.md) |
| 现在有哪些能力、规模与版本基线 | [当前阶段状态](planning/current-handoff.md) |
| 还差什么、哪些要验收、哪些待决策 | [待办与待决策](planning/todo.md) |
| 后面怎么排 | [路线图](planning/roadmap.md)（剩余 S8/S9 与门槛） |
| 我要改代码，从哪读起 | [AI 阅读引导](architecture/ai-reading-guide.md) |
| 整体分层、生命周期、并发、取舍 | [系统架构](architecture/overview.md) |
| 某个领域的契约 | 下面的模块文档 |
| 怎么检查、怎么验收、性能门槛 | [验证策略](guides/validation.md)、[性能基准](guides/performance.md) |
| 以前怎么做的 | [规划归档](planning/archive/README.md) |

## 领域模块

- [图库与元数据](modules/library.md)：单库生命周期、设置、schema 与迁移、文件夹、临时文件夹、操作记录与撤销、命令契约。
- [导入与缩略图](modules/import.md)：输入与校验、BLAKE3、pHash、主色、重复策略、导入即压缩、缩略图包与 `thumb://`。
- [浏览与检索](modules/browse-and-query.md)：查询 DTO、搜索、筛选与分面计数、排序与游标、视图与选择、详情与批量编辑、找相似、失效规则。
- [查看器与几何编辑](modules/viewer-and-editor.md)：查看与导航、几何组合、保存语义、体积预估、`img://`。
- [AI 服务](modules/ai-service.md)：进程托管、`/v1` 接口、feature pack、批量任务、本地检索与相似度算法。
- [维护与批量工作流](modules/maintenance.md)：缩略图重建、待释放与清理、图片空间优化（估算与执行）、后续维护项。
- [MCP 图库控制](modules/mcp.md)：传输与进程、24 个工具、selector、数量核对、三档权限与确认队列、审计。

## 规范

- [验证策略](guides/validation.md)：允许与禁止的检查、按改动选检查、真实窗口验收步骤、AI 服务端一次性检查。
- [性能基准](guides/performance.md)：方法、实测记录、结论、预算与仍需复测项。

## 目录分工

- `architecture/`：跨模块的长期约束与取舍。
- `modules/`：领域契约、数据模型、行为与边界。
- `guides/`：验证与性能。
- `planning/current-handoff.md`：只写“现在”，不写流水。
- `planning/todo.md`：唯一未完成清单。
- `planning/roadmap.md`：只写还没做的阶段。
- `planning/archive/`：已完成批次与被替代的计划，索引见[归档说明](planning/archive/README.md)。

## 维护规则

- 契约变化时同步改 `architecture/` 或 `modules/` 的对应段落，并在 `todo.md` 记下未完成或未验收的部分。
- 新功能落地后写“已实现”只写到 `current-handoff.md` 的能力现状与模块契约里，不写逐批流水；批次记录、与计划的差异、实测证据放 `planning/archive/`。
- 一个数字只在一处维护：性能数字在 `guides/performance.md`，预算在 `roadmap.md`，版本与规模基线在 `current-handoff.md`。
- 不写版本小节、CHANGELOG、法律文本；不写“计划实现”当成“已验收”。

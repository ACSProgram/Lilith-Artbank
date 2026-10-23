# Lilith Artbank 文档

本项目是内部工具，文档只分两层层级：`architecture/` 与 `modules/` 记录当前有效的实现约束；`planning/` 记录当前阶段、未完成事项与路线。没有面向公众的发布、法务或版本材料。

## 入口

- [系统架构](architecture/overview.md)：分层、数据布局、并发、性能原则、AI 服务边界。
- [AI 阅读引导](architecture/ai-reading-guide.md)：代码任务的第一步，按问题选路由。
- [当前阶段状态](planning/current-handoff.md)：现在做到哪里、验证过什么。
- [待办与待决策](planning/todo.md)：唯一未完成清单。
- [分阶段路线图](planning/roadmap.md)：阶段目标、代码预算与验收门槛。

## 领域模块

- [图库与元数据](modules/library.md)：设置、图库生命周期、SQLite schema、命令契约。
- [导入与缩略图](modules/import.md)：解码、BLAKE3、pHash、主色、缩略图包。
- [浏览与检索](modules/browse-and-query.md)：网格/列表、键集分页、筛选排序、集合、回收站。
- [查看器与图像处理](modules/viewer-and-editor.md)：查看、裁剪/旋转/调尺寸、色彩/LUT/DFT/卷积。
- [AI 服务](modules/ai-service.md)：进程托管与 `/v1` HTTP 接口。
- [维护与批量工作流](modules/maintenance.md)：清理、缩略图重建、描述导入导出。

## 规范

- [验证策略](guides/validation.md)：允许的检查、界面验收边界、真实仓库只读。
- [性能基准](guides/performance.md)：原始基线、实测结果、运行方法、门槛。

## 目录分工

- `architecture/`：跨模块的长期约束。
- `modules/`：领域契约、数据模型、行为与边界。
- `guides/`：验证、性能与工程规范。
- `planning/current-handoff.md`：当前批次状态与基线，只写“现在”。
- `planning/todo.md`：未完成、未验证或待决策事项。
- `planning/roadmap.md`：阶段划分与验收标准。
- `planning/archive/`：已归档的分析、批次记录与被替代的计划；不再作为执行依据。

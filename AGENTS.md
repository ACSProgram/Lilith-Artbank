# Lilith Artbank 工作约定

## 先选择上下文入口

代码任务先读 `docs/architecture/ai-reading-guide.md` 选择路由；不要默认通读整个仓库。

- 图库打开/创建/关闭、设置、SQLite schema：读 `docs/modules/library.md`，再进入 `src-tauri/src/app/`、`src-tauri/src/library/`。
- 导入、哈希、pHash、主色、缩略图包：读 `docs/modules/import.md`，再进入 `src-tauri/src/import/`、`src-tauri/src/imageops/`。
- 网格/列表、分页、筛选、排序、集合、回收站：读 `docs/modules/browse-and-query.md`，再进入 `src/modules/library/`、`src-tauri/src/library/`。
- 查看器、裁剪/旋转/调尺寸、图像处理：读 `docs/modules/viewer-and-editor.md`，再进入 `src/modules/viewer/`、`src/modules/editor/`、`src-tauri/src/imageops/`。
- description / embedding / pose、AI 服务进程：读 `docs/modules/ai-service.md`，再进入 `src-tauri/src/ai/`、`src/modules/ai/`。
- 清理、缩略图重建、描述导入导出：读 `docs/modules/maintenance.md`。
- 构建、测试、性能基准：读 `docs/guides/validation.md`、`docs/guides/performance.md`。
- 当前阶段状态与未完成事项：读 `docs/planning/current-handoff.md`、`docs/planning/todo.md`；阶段划分看 `docs/planning/roadmap.md`。

## 不可动摇的决策

- 元数据只走 SQLite；不要把整库元数据载入内存或一次性发给前端。页面查询使用键集游标。
- 原图按 BLAKE3 内容哈希命名；图片 id 是稳定代理键。编辑产生新内容哈希文件，标签/集合引用 id。
- 缩略图写追加式 `.pack`，数据库只记录 segment/offset/length；不要改成 20 万个独立小文件。
- 浏览器只解码当前页缩略图；Rust 不做 DDS/BC 缩略图管线。
- AI 能力统一走本机 HTTP 服务；应用负责进程生命周期。接口以 `docs/modules/ai-service.md` 为准。
- 不做 PinBoard、ImagePaste、PasteBoard、调色盘、色彩测试、截屏小工具。
- 不做公开版本号、CHANGELOG、LICENSE/法务材料；不要为这些建文件。
- 应用不读旧 `.data`（`image_data.bin` / `previews.pak`）；旧库迁移是仓库外的独立一次性脚本。

## 边界

- `src/modules/<module>/api.ts` 是该领域 Tauri 命令的唯一前端入口。
- `src/modules/<module>/types.ts` 只放边界 DTO；纯前端计算放模块自己的 model/helper。
- 前端模块之间不互相导入；跨模块流程在 `src/app/` 编排。
- `src-tauri/src/app/` 管设置、状态、命令适配与生命周期；`library/` 管 schema、连接池与仓储；`imageops/` 只做像素；`ai/` 只做 HTTP 与任务。
- Tauri 命令是薄适配器：解析 DTO、取当前图库、调用领域函数、把错误转成字符串；不在命令里写业务。
- 所有文件系统、数据库与模型操作都在 Rust 完成。前端不直接读图库目录。
- SQLite 写入必须单写连接；读取走连接池。长任务必须可取消并有进度事件。

## 性能与规模

- 目标规模：20-50 万张图片、数百 GB。
- 门槛：一页 150 张缩略图从请求到可见 ≤1s（硬上限 3s）；首屏元数据查询 ≤50ms；启动打开库 ≤1s；内存不随库大小线性增长。
- 任何引入“全量扫描/全量载入/每帧磁盘 IO”的方案必须先看 `docs/guides/performance.md` 并给出基准。
- 新增热点路径必须跑对应基准；不要用墙钟跨机器比较。

## 提问与决策

- 会实质影响行为、性能或架构的选择，先向用户提问，用直白的话说明后果。
- 有明显最优解的直接做并说明理由；拿不准是否值得问时，优先做可逆的小选择并标注。
- 一次聚焦一个问题，有推荐项时先给推荐项。

## 文档

- 当前有效事实写 `docs/architecture/`、`docs/modules/`、`docs/guides/`。
- `docs/planning/current-handoff.md` 只写当前批次状态与基线；`docs/planning/todo.md` 只写未完成、未验证与待决策。
- 已完成批次与已被替代的计划搬入 `docs/planning/archive/`，同步更新 `docs/planning/archive/README.md`，并从表层删除。
- 代码入口或契约变化时同步更新模块文档。不要把“计划实现”写成“已经验收”。
- 不写版本小节、不写 CHANGELOG、不写法律文本。

## 验证

- 默认跑与改动匹配的检查：`npm run typecheck`、`npm run build`、`cargo fmt --check`、`cargo check`、`cargo test`、`git diff --check`。
- 前端改动必须跑 `npx tsc --noEmit`；Rust 改动必须跑 `cargo test`。
- 不自动编译 release 产物，不启动真实窗口做输入自动化；界面最终由用户手工验收。
- 允许用无输入注入的无头浏览器/浏览器预览做静态渲染、DOM 结构与布局检查；不要用 `SendInput`、鼠标键盘模拟或注入消息驱动真实应用窗口。
- 真实仓库 `G:\Lilith Image Library` 只读；测试、基准和演示一律用临时目录或复制出来的小样本。

## Git

- **代理自己提交，不要等用户提醒**：每个边界清晰、通过对应验证的阶段或任务结束时，代理主动核对改动、暂存相关文件并创建提交。
- 提交前核对一次 `git status` 与 diff，不夹带无关文件；用户已有的未完成改动不要顺手提交。
- 提交信息可以用中文，也可以沿用英文祈使句；主题简洁说明行为变化，正文写原因与验证结果。
- 不使用 `type(scope):` 前缀。代理不自动 `git push`；是否推送由用户决定。
- 不为了整洁回退用户改动；发现意外状态先调查再处理。

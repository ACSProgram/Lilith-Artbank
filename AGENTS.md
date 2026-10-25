# Lilith Artbank 工作约定

## 先选择上下文入口

代码任务先读 `docs/architecture/ai-reading-guide.md` 选择路由；不要默认通读整个仓库。

- 图库打开/创建/关闭、设置、SQLite schema：读 `docs/modules/library.md`，再进入 `src-tauri/src/app/`、`src-tauri/src/library/`。
- 操作记录、撤销、待释放文件：读 `docs/modules/library.md` 的“操作记录与撤销”，再进入 `src-tauri/src/library/history.rs`、`src-tauri/src/app/state.rs`、`src/modules/library/OperationHistoryPanel.tsx`。
- 导入、哈希、pHash、主色、缩略图包：读 `docs/modules/import.md`，再进入 `src-tauri/src/import/`、`src-tauri/src/imageops/`。
- 网格/列表、分页、筛选、排序、集合、回收站：读 `docs/modules/browse-and-query.md`，再进入 `src/modules/library/`、`src-tauri/src/library/query.rs`、`src-tauri/src/library/`。
- 查看器、裁剪/旋转/调尺寸、图像处理：读 `docs/modules/viewer-and-editor.md`，再进入 `src/modules/viewer/`、`src/modules/editor/`、`src-tauri/src/imageops/`。
- description / embedding / pose、AI 服务进程：读 `docs/modules/ai-service.md`，再进入 `src-tauri/src/ai/`、`src/modules/ai/`。
- AI 服务端与便携打包：读 `docs/modules/ai-service.md` 的“服务端实现”，再进入 `server/`、`tools/package-portable.mjs`、`src-tauri/src/app/paths.rs`。
- 找相似、相似分组、pHash/主色/embedding/pose 相似度：读 `docs/modules/ai-service.md` 的“本地检索”，再进入 `src-tauri/src/library/similarity.rs`、`src-tauri/src/ai/pose_similarity.rs`、`src-tauri/src/ai/embedding.rs`、`src/app/useSimilarityBrowse.ts`。
- 清理、缩略图重建、描述导入导出：读 `docs/modules/maintenance.md`。
- 图片空间优化（估算与执行）：读 `docs/modules/maintenance.md` 的“图片空间优化”，再进入 `src-tauri/src/imageops/optimize.rs`、`src-tauri/src/library/optimize_estimate.rs`、`src-tauri/src/library/optimize.rs`、`src-tauri/src/app/state.rs`、`src/app/OptimizeDialog.tsx`。
- 构建、测试、性能基准：读 `docs/guides/validation.md`、`docs/guides/performance.md`。
- 当前阶段状态与未完成事项：读 `docs/planning/current-handoff.md`、`docs/planning/todo.md`；阶段划分看 `docs/planning/roadmap.md`。

## 不可动摇的决策

- 元数据只走 SQLite；不要把整库元数据载入内存或一次性发给前端。页面查询使用键集游标。
- 会改变图库内容的写路径必须接入操作记录（`history::write` 或 live operation）；永久删除与清空回收站只写待释放标记，原图由释放任务删除。
- 原图按 BLAKE3 内容哈希命名；图片 id 是稳定代理键。编辑产生新内容哈希文件，标签/集合引用 id。
- 缩略图写追加式 `.pack`，数据库只记录 segment/offset/length；不要改成 20 万个独立小文件。
- 浏览器只解码当前页缩略图；Rust 不做 DDS/BC 缩略图管线。
- AI 能力统一走本机 HTTP 服务；应用负责进程生命周期。接口以 `docs/modules/ai-service.md` 为准。
- 设置、日志与相对资源路径都以程序根目录为准（`src-tauri/src/app/paths.rs`）；不读旧 AppData 设置，也不做迁移。
- Python 环境与模型文件不打包、不下载；只产出便携文件夹（`npm run pack:portable`），不生成 NSIS 安装器。
- 姿势相似度按旧程序 `MediaPipePoseData::similarity` 1:1 移植：权重、landmark 索引与“镜像取较大值”都不许自行改动，`ai/pose_similarity.rs` 的 golden 值测试必须保持通过。
- pHash 与 4/8 主色常驻内存只用于相似度扫描；全库相似对必须走分段索引收窄候选，不允许直接 O(n²) 两两比较。
- 不做 PinBoard、ImagePaste、PasteBoard、调色盘、色彩测试、截屏小工具。
- 不做公开版本号、CHANGELOG、LICENSE/法务材料；不要为这些建文件。
- 应用不读旧 `.data`（`image_data.bin` / `previews.pak`）；旧库迁移是仓库外的独立一次性脚本。

## 边界

- `src/modules/<module>/api.ts` 是该领域 Tauri 命令的唯一前端入口。
- `src/modules/<module>/types.ts` 只放边界 DTO；纯前端计算放模块自己的 model/helper。
- 前端模块之间不互相导入；跨模块流程在 `src/app/` 编排。
- `src-tauri/src/app/` 管设置、状态、命令适配与生命周期；`library/` 管 schema、连接池与仓储；`imageops/` 只做像素；`ai/` 只做 HTTP 与任务；`platform/` 只放剪贴板等平台差异。
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

## 界面文案

- 写小字提示前先问两句：**用户需要这条吗？它是在帮用户理解，还是在展示某项技术、要求或原理？** 后者直接删。
- 只写会改变用户决策的信息：后果（会不会删除/覆盖/复制文件）、范围（影响哪些图片）、不可逆性、生效时机、数据去哪了。
- 不解释实现：不写“不需要先滚动加载”“按 id 引用”“分段索引”“写连接只有一条”“滚动才解码”这类给同行看的说明。
- 单位、范围、当前值放进控件（滑块右侧的数值、输入框的 min/max、占位符），不要另起一行解释。
- 一个控件最多一句提示；需要两句以上，说明这条提示本身该拆开或者该删。

## 验证

- 默认跑与改动匹配的检查：`npm run typecheck`、`npm run build`、`cargo fmt --check`、`cargo check`、`cargo test`、`git diff --check`。
- 前端改动必须跑 `npx tsc --noEmit`；Rust 改动必须跑 `cargo test`。
- **不编译 release**：不跑 `cargo build --release`、`tauri build`，也不为了“确认能打包”而全量编译。需要 release 产物时由用户自己执行。
- **不做 UI 调试**：不启动 `npm run dev` / `npm run tauri dev`，不打开浏览器预览或无头浏览器，不截图、不抓 DOM、不注入输入、不模拟鼠标键盘、不驱动真实窗口。
- 界面、布局、交互、观感一律由用户手工验收；代理只负责代码与自动检查，并把“怎么验”写清楚交给用户。
- 真实仓库 `G:\Lilith Image Library` 只读；测试、基准和演示一律用临时目录或复制出来的小样本。

## 收尾

- 完成任务后不允许打开、展示或预览应用界面；不要留下常驻的开发服务器、真实窗口、浏览器页面或后台任务。
- 最终只报告改动、检查结果与用户需要执行的命令，由用户自己打开应用测试。

## Git

- **代理自己提交，不要等用户提醒**：每个边界清晰、通过对应验证的阶段或任务结束时，代理主动核对改动、暂存相关文件并创建提交。
- 提交前核对一次 `git status` 与 diff，不夹带无关文件；用户已有的未完成改动不要顺手提交。
- 提交信息可以用中文，也可以沿用英文祈使句；主题简洁说明行为变化，正文写原因与验证结果。
- 不使用 `type(scope):` 前缀。代理不自动 `git push`；是否推送由用户决定。
- 不为了整洁回退用户改动；发现意外状态先调查再处理。

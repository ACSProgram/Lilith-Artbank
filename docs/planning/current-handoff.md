# 当前阶段状态

日期：2026-10-27

本文件只写“现在”：当前生效的能力、基线与约束。未完成、未验收与待决策事项只写在[待办与待决策](todo.md)；后续阶段见[路线图](roadmap.md)；历史记录见[规划归档](archive/README.md)。没有进行中的批次。

## 能力现状

每条只给结论与契约入口；实现细节在对应模块文档里。

- **图库与元数据**：单库会话，设置 v7（只存一个 `libraryPath`），schema v8（v1→…→v8 链式迁移），多级文件夹与磁盘目录一一对应，临时文件夹为会话级暂存区，操作记录与线性撤销/重做（线性回退、灰色重做分支、延迟释放）。见[图库与元数据](../modules/library.md)。
- **导入**：文件/目录/拖放输入，BLAKE3 内容寻址与原子发布，EXIF 方向，512px WebP 缩略图写入追加式 pack，pHash 与 4/8 主色，精确/批内/感知三类重复，进度-取消-失败清单，可选的导入即压缩。见[导入与缩略图](../modules/import.md)。
- **浏览与检索**：键集游标与 id 列表游标分页，7 个排序指标、最多 7 级，搜索（描述/标签名/文件夹路径，当前视图或全库），筛选分面计数，集合 CRUD 与集合运算，回收站；所有写入统一走 `DataChange` + `planDataChange` 决定失效范围，刷新保留旧行并按活动图锚定滚动。见[浏览与检索](../modules/browse-and-query.md)。
- **查看器与编辑**：全屏查看、缩放平移、胶片带、姿势叠加；翻转→旋转→裁剪→缩放可组合，输出 JPEG/PNG/WebP，保存走“新内容发布”（新哈希文件、就地更新行、旧图暂存供撤销）。见[查看器与几何编辑](../modules/viewer-and-editor.md)。
- **相似与语义检索**：pHash、4/8 主色、embedding、pose 四类找相似与全库相似分组；语义搜索用打开图库时算出的全库均值做中心化余弦，向量 mmap 扫描。见[AI 服务](../modules/ai-service.md#本地检索)。
- **AI 服务**：本机 `/v1` HTTP 服务的进程托管（探活、退避重启、优雅退出），feature pack 存 embedding（fp16）与 pose（f32），批量任务挂任务总线。描述不实现，`/v1/describe` 返回 404。见[AI 服务](../modules/ai-service.md)。
- **任务与并发**：`app/tasks.rs` 任务总线，`write` / `ai` / `scan` 三条 lane；长任务同 lane FIFO 排队、写优先，短写快速失败；撤销/重做与清空记录不排队；panic、线程起不来与提前返回都落到失败终态并释放 lane。
- **图片空间优化**：只读估算 → 确认执行两阶段，分层切片抽样与按层外推，执行前三道复核（参数指纹、候选集合指纹、逐张 `stat`），不可撤销的 `optimize` 记录。见[维护与批量工作流](../modules/maintenance.md#图片空间优化)。
- **MCP 图库控制**：应用内 stateless Streamable HTTP 服务（`127.0.0.1` + Bearer 口令 + `mcp-runtime.json`）与同 exe 的 `--mcp-stdio` 小桥；24 个工具、三档权限、10 分钟确认队列、`logs/mcp.log` 审计。永久删除、清空回收站、空间优化、像素编辑与普通库批量 AI 生成不开放。见[MCP 图库控制](../modules/mcp.md)。
- **前端契约**：`src/modules/<module>/api.ts` 是命令唯一入口，跨模块编排在 `src/app/`，主题只走 `src/styles/tokens.css` 的语义变量，破坏性操作确认文案统一在 `src/shared/confirm.ts`。

## 基线

- 图库：主库 `G:\Lilith Image Library`（390 542 张、445 个普通文件夹 + 1 个临时文件夹、556 个标签（175 个有图）、81 649 条 pose、9 个缩略图段、345 541 条 embedding、`library.db` 991.8 MB），独立测试库 `G:\Lilith Test Library`（9 455 张）；失败内容在 `G:\Lilith Image Library Failed`（247 条超过 1 亿像素）。
- 压缩候选：主库 274 068 张 / 467.44 GB / 1 939.08 GP；默认 200 张档位只覆盖候选体积的 0.078%，`s5`（有损源 bpp ≥ 0.50）一层占候选体积 82.8%。
- 存储版本：schema v8、设置 v7。设置、主日志与 `ai-server.log` 都在程序目录，不读旧 AppData 设置；MCP 运行时文件是程序目录的 `mcp-runtime.json`（本地凭据，已 gitignore）。
- 自动检查（文档整理前一轮）：`cargo fmt --check`、`cargo clippy --all-targets`（0 警告）、`cargo test --lib`（372 通过、9 ignored）、`npx tsc --noEmit`、`npm run test:web`（54 通过）、`npm run build`、`git diff --check`。
- 排序指标 7 个（导入时间 / 修改时间 / 评分 / 文件大小 / 像素尺寸 / 文件夹 / 集合内顺序），最多 7 级；单级且非文件夹走键集游标，其余走进程内有序 id 列表。
- 性能数字不在这里重复：方法与全部实测见[性能基准](../guides/performance.md)。

## 约束

- `G:\Lilith Image Library` 已由迁移器改造为 Artbank 库，旧 `.data` 已删除，不要再让旧程序写入该目录。
- 应用不读旧 `.data`；迁移器在仓库外 `research/library-migration`，不进入 `src-tauri` 与产品提交历史。
- 真实库只用于验收；测试、基准和演示仍使用临时目录或复制样本，避免写入真实库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏；不做公开版本号、CHANGELOG 与法务材料。
- 界面与观感由用户手工验收，代理不启动应用、不截图、不驱动窗口；验收清单见[待办](todo.md)。

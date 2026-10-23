# 当前阶段状态

日期：2026-10-23
阶段：**S5 已取消；S6 已实现，自动验收通过；真实 AI 服务与 Tauri 界面待用户手工验收**

历史批次（S0–S3）的实现与验证记录已移入 [归档](archive/2026-10-23-S0-S3-实现记录.md)；查询引擎与缩略图基线仍以 [性能基准](../guides/performance.md) 为准。

## 本批次前提

- 跳过 S5（色彩/LUT/DFT/卷积），原因与作废计划见 [S5 取消与 AI 模型调整](archive/2026-10-23-S5取消与AI模型调整.md)。
- image/text embedding 改为 `siglip2-giant-opt-patch16-384`，1536 维，落盘 fp16；旧 CLIP 向量不兼容、不参与检索。
- 语义检索在打开图库时计算全库 embedding 均值，查询使用 `cosine(v - mean, q - mean)`；会话内不实时更新。
- 向量包 mmap 分块扫描，不整体载入内存；结果只回 id + 分数。
- pose 继续用 MediaPipe Pose，S7 相似度按原程序 `ZyImageAlgorithm.cpp` 1:1 移植，不改权重与公式。
- description 换模型但 `/v1/describe` 契约不变；本批只做客户端，不实现服务端。

## 本批次完成：S6 AI 客户端与语义搜索

Rust：

- `ai/client.rs`：`/v1/health`、`/v1/models/ensure`、`/v1/embed/image`、`/v1/embed/text`、`/v1/pose`、`/v1/describe`、可选 `/v1/shutdown`；连接超时 5s，embedding/pose 60s、description 300s；网络错误与 5xx 重试 2 次，4xx 不重试。
- `ai/process.rs` + `ai/runtime.rs`：`executablePath` 启动、`--port`、健康检查、最多 3 次退避重启、`autoStart`、手动启动/停止、退出时优雅关闭后终止；外部服务只连接不接管。
- `ai/pack.rs`：`features/<kind>-<model>-<dim>-<dtype>-<hash>.pack` 追加式写入，32 字节包头（magic/version/kind/dtype/dim/record_len/created_at）。
- `ai/embedding.rs`：服务返回向量再归一化后存 fp16；打开图库后台计算全库 fp32 均值；检索使用 `cosine(v - mean, q - mean)`，mmap + rayon 分块扫描 + top-K；`feature_blobs` 只存指针。
- `ai/pose.rs`：33 点 × (x,y,z,visibility) 存 f32；空结果按 33 个零置信点保存。
- `ai/job.rs`：embedding/pose/description 批量任务；按选中/当前视图/全库解析 id；默认只补缺失，`overwrite` 只重算非手写描述；任务可取消，进度 150ms 节流，失败清单最多 200 条；连续 5 次失败停止。
- `ai/input.rs`：embedding/pose 使用 512px 缩略图包，缺失时回退解码；description 解码原图并生成最长边 1536 的 WebP。
- `metadata::set_ai_description`：写 `description_source='ai'`、`description_model`；空描述可填，非空手写描述永不覆盖。
- 新增命令：`get_ai_status`、`ai_ensure_models`、`ai_start_service`、`ai_stop_service`、`start_ai_job`、`cancel_ai_job`、`get_ai_job_status`、`ai_describe_image`、`ai_generate_pose`、`semantic_search`、`get_similarity_page`、`get_similarity_ids`；`get_image_pose` 改为读取 feature pack。
- `query::rows_by_ids`：语义结果按 rank 顺序回填完整行。
- 图库打开/关闭接入 AI 运行时的均值与语义缓存；编辑、移入/恢复/删除/清空回收站会使语义缓存失效；AI 任务运行期间禁止关库/导入/编辑。

前端：

- `src/modules/ai/`：types、api、AI 状态轮询、任务对话框；服务状态、模型加载、范围选择、覆盖选项、进度/取消/失败清单。
- 图库工具栏：AI 状态胶囊、AI 任务入口、语义搜索面板与结果模式；语义模式禁用普通筛选/排序，Ctrl+A 选择全部语义结果。
- 详情侧栏：单张“AI 生成描述”，成功后立即显示并刷新。
- 查看器：无 pose 时可“生成姿势骨架”，有 pose 时切换叠加；返回 33 点为零时提示未检测到人体。
- 设置页：AI 服务检测/启动/停止/加载模型与 health 能力展示。

## 验证结果（S6）

```text
npm run typecheck
npm run build
cargo fmt --check
cargo clippy --all-targets   # 无警告
cargo test --lib             # 99 passed, 5 ignored
```

新增测试：

- 客户端：health/embedding 解析与归一化、5xx 重试、4xx 不重试、describe 查询参数编码、pose 33 点。
- job：mock 服务下 embedding/pose/description 端到端；手写描述在 overwrite 下仍不被覆盖。
- pack：追加/读取/越界/包头一致性；pose 缺失点补零。
- embedding：均值中心化排序、回收站图片排除、top-K 只返回前 K 条且按分数排序。
- 基准：20 万条 × 1536 维 fp16 单 pack 扫描 449.26 ms（开发档），低于 500 ms 预算。

浏览器静态检查未能完成：内置浏览器导航 `http://127.0.0.1:5173` 连续超时、页面为空白；未使用真实窗口或输入自动化。语义/任务界面的实际布局仍需用户在 `npm run tauri dev` 中手工验收。

本轮收尾整理（不改行为契约）：

- 客户端：去掉恒为空的 `normalized`/`truncated` 占位逻辑（截断改为记日志）；`with_retry` 去掉不可达的兜底分支；`describe` 的查询参数构造简化；百分号编码不再逐字节 `format!`；L2 归一化收敛到 `embedding::normalize_f32` 一处实现。
- 检索：top-K 改为 `select_nth_unstable_by` 部分选择后再排序，与文档一致，不再为取 K 条对全量结果排序；新增对应测试。
- 运行时：`MeanSlot` 增加 `ready/empty/failed` 构造，均值状态的四处字段赋值收敛；`status()` 只取一次进程锁；监督线程改用 `SupervisorGuard` 按代次释放标记，避免旧线程清掉新线程的 `supervising`；启动次数上限提取为 `process::MAX_START_ATTEMPTS`。
- 任务：`existing_features` 的 kind 改为绑定参数、分支合并；描述状态改为 `DescriptionState`；描述长度默认值收敛为 `job::DEFAULT_DESCRIBE_TOKENS`；去掉重复的失败路径克隆。
- 命令：`ai_stop_service` 改为异步 + 阻塞线程池，停止服务不再卡住 IPC 线程。
- 前端：`aiApi` 用 `requireTauri`/`offlineStatus()` 去掉 13 处重复分支；语义结果与图库查询在 `LibraryView` 合并为单一 `listSource`；`useSemanticSearch.retry` 去掉走不到的 token 分支；任务对话框重新打开时接管 `status.job`，并补上阶段中文标签；设置页去掉“接入后……”等过期文案。
- 文档：修正 pack 命名（含 hash 后缀）、pose 的 dim 语义、进度节流 150ms、启动失败重试与“运行中崩溃不自动重启”的实际行为、`features/*.bin` 笔误；路线图标记 S6 已实现；清理交接文档中 S5/S6 的过期结论。

## 本批次完成：S4 查看器与几何编辑

Rust：

- `imageops/edit.rs`：翻转 → 顺时针旋转 → 裁剪 → Lanczos3 缩放的几何算子；JPEG/PNG/WebP 编码，JPEG 透明合成白底，编辑上限 5000 万像素/单边 65535。
- `library/viewer.rs`：`get_viewer_source`。小图直接由浏览器解码；>2400 万像素、TIFF/TGA 或带 EXIF 方向的图片生成最长边 4096 的 WebP 预览，缓存于 `.artbank/temp/viewer/`。
- `library/edit.rs`：保存采用“新内容发布”事务——临时编码 + BLAKE3 + `sync_all`、冲突检查、原子发布、追加缩略图、更新 `images` 行并清空 `feature_blobs`，提交后删除旧原图与旧预览；标签/评分/描述/集合保留。
- `library/content.rs`：原图行加载与路径解析。
- `app/img_protocol.rs`：`img://`（Windows 为 `http://img.localhost`）提供原图与预览，带 immutable 缓存与内容哈希 ETag。
- `platform/clipboard.rs`：`copy_image` 解码、应用 EXIF 方向后写入系统剪贴板。
- 新增命令：`get_viewer_source`、`save_image_edit`、`get_image_pose`（S6 前返回 null）、`copy_image`、`window_toggle_fullscreen`。保存与导入/图库切换共用维护守卫，并在阻塞线程池执行。

前端：

- `src/modules/viewer/`：types、api、model、`ImageViewer`、`ViewerTools`、`PoseOverlay`。适应窗口/1:1/滚轮缩放、拖拽平移、双击切换、上下张与翻页加载、全屏、键盘快捷键、裁剪选区、输出格式与质量。
- `src/app/ViewerHost.tsx`：查看器与可编辑详情侧栏的编排；评分/标签/描述/文件夹直接复用 Inspector，回收站/恢复/永久删除成功后关闭查看器。
- `src/shared/viewerSession.ts`：图库向查看器提供已加载条目、翻页、选择同步与元数据操作的桥接契约。
- `src/shared/imgUrl.ts`：`img://` URL 生成。
- 入口：双击网格/列表、Enter、右键“打开查看器”；网格右键新增“复制图片”。
- 拖出到资源管理器（Windows OLE）按用户确认暂缓，保留“复制图片”与“复制文件路径”。

## 验证结果

已运行并通过：

```text
npm run typecheck
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets   # 无警告
cargo test --manifest-path src-tauri/Cargo.toml --lib   # 83 passed, 4 ignored
git diff --check                                        # 无空白问题
```

S4 新增测试覆盖：

- 几何算子：翻转先于旋转、旋转后裁剪坐标、裁剪后缩放、非法参数拒绝；三种输出格式可解码，PNG 保留 alpha，JPEG 透明合成白底。
- 保存事务：宽高交换、旧原图删除、新文件存在、缩略图指针可读、`thumb_version` 递增；标签/评分/描述/集合保留、`feature_blobs` 清空；哈希冲突、无修改与回收站图片被拒绝。
- 查看器源：小图直接服务、预览生成与复用、4096 长边上限、EXIF 方向 6 生成转正预览、原图缺失报错、回收站只读。

本轮收尾整理（不改行为契约）：

- 修复 `img://` 预览响应的 `Content-Type`（`webp` → `image/webp`）；图片行缺失改为 404，与前端失败态一致。
- 查看器打开时把焦点移进覆盖层，避免网格容器级键盘处理与查看器同时响应 ←/→、Esc；覆盖层补 `aria-modal`。
- 去掉重复的 URL 重试参数、未使用的 `zoomPercent`/`activeItem`、`has_changes` 的冗余参数；姿势可见性阈值收敛到 `hasVisiblePose`。
- `AppState` 的图库句柄统一走一次取锁；`TempFile` 改为 `Option<PathBuf>`；剪贴板打开逻辑收敛为一个 helper。
- 文档同步：架构分层与存储布局补 `img://`、`temp/viewer/`；修正 `img://` 状态码说明与 `temp` 缓存无上限的待决策项。

浏览器预览（Chromium 代理，724×712 CSS，mock 数据）：

- 查看器覆盖工作区，工具面板 262px、画布与 320px 侧栏无溢出；适应缩放 30%，滚轮 30%→40%，双击回到适应。
- 旋转点亮“未保存”；裁剪拖拽 306px 屏幕距离映射为 1,024×1,024 原图像素；调尺寸 512、重置、上下张与 Inspector/网格选择联动、侧栏开关、关闭均正常；无控制台错误。
- 该检查不代表 WebView2 的图片解码、剪贴板、全屏或保存性能。

## 未验证

- 真实 Tauri 窗口的 S1 导入、S2/S3 浏览与 S4 查看器全流程（含全屏、键盘、侧栏编辑）。
- 真实 WebView2 的原图/大图预览加载、EXIF 方向图显示与裁剪一致性、缩放平移帧率。
- 真实图库的保存 round-trip：旧原图删除、`img://` 与缩略图立即刷新、pHash/主色更新、embedding/pose 清除。
- 4K/8K 保存耗时与内存峰值；超过 5000 万像素的只读限制与拒绝提示。
- 真实 Windows 程序粘贴剪贴板图片；透明图与 EXIF 方向。
- 深分页中打开查看器后保存会刷新回第一页，上下张上下文会重置（已知限制）。
- 真实 AI 服务：health/ensure/embed/pose/describe 全链路、模型维度校验与失败文案。
- 真实窗口 AI 任务进度、取消、失败清单与手写描述保护；语义搜索分页与 Ctrl+A。
- 真实窗口查看器姿势生成/叠加与详情侧栏 AI 描述。
- 文件夹移动、永久删除、清空回收站仍无进度事件与取消（S8/S9 前补齐）。
- 20 万行滚动、150 张缩略图从请求到可见、冷/热缓存与 WebView2 内存行为。
- release 构建的 exe 与 DLL 清单（S9）。

## 下一步

1. 用户手工验收：`npm run tauri dev` → 打开图库 → 设置页检测/启动 AI 服务并加载模型 → 运行 embedding/pose/description 批量任务（含取消与失败清单）→ 语义搜索与翻页 → 查看器生成姿势骨架 → 详情侧栏“AI 生成”。
2. 同时复验 S1–S4 的真实窗口流程（导入、浏览、编辑、查看器保存、剪贴板）。
3. 验收通过后进入 S7：pHash/颜色/向量/pose 相似度与高级集合。
4. 待决策项见 [待办与待决策](todo.md)，尤其是 AI 服务崩溃是否自动重启、语义搜索是否叠加筛选、OLE 拖出与编辑上限的进度/取消。

## 约束

- `G:\Lilith Image Library` 未修改；所有测试、基准与样本使用临时目录。
- 不读旧 `.data`；迁移脚本不在本仓库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏。

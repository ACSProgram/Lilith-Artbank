# 当前阶段状态

日期：2026-10-23
阶段：**S4 已实现，自动验收通过；S1–S4 的真实 Tauri 窗口界面待用户手工验收**

历史批次（S0–S3）的实现与验证记录已移入 [归档](archive/2026-10-23-S0-S3-实现记录.md)；查询引擎与缩略图基线仍以 [性能基准](../guides/performance.md) 为准。

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
- 姿势叠加 UI 已就绪，但 `get_image_pose` 要到 S6 的 feature pack 才能返回真实 landmark。
- 文件夹移动、永久删除、清空回收站仍无进度事件与取消（S8/S9 前补齐）。
- 20 万行滚动、150 张缩略图从请求到可见、冷/热缓存与 WebView2 内存行为。
- release 构建的 exe 与 DLL 清单（S9）。

## 下一步

1. 用户手工验收：`npm run tauri dev` → 打开图库 → 在网格/列表打开查看器 → 缩放/平移/全屏 → 旋转、翻转、裁剪、调尺寸 → 保存并确认缩略图与原图更新、标签/评分/集合保留 → 复制图片并粘贴到其他程序。
2. S4 验收通过后进入 S5：色彩/LUT/DFT/卷积处理链与 Rust 降采样预览。
3. 待决策项见 [待办与待决策](todo.md)，尤其是 OLE 拖出、深分页保存刷新与编辑上限的进度/取消。

## 约束

- `G:\Lilith Image Library` 未修改；所有测试、基准与样本使用临时目录。
- 不读旧 `.data`；迁移脚本不在本仓库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏。

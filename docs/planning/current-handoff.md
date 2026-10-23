# 当前阶段状态

日期：2026-10-23
阶段：**S2 已实现，自动验收通过；真实窗口的 S1 导入与 S2 浏览界面待用户手工验收**

## 已完成

### S0 工程骨架

- Tauri 2 + React 19 + TypeScript + Vite 6 工程。
- 无边框自绘标题栏、Lilith Artworks 风格浅色应用外壳、侧栏、欢迎页、设置弹窗、新建/打开图库弹窗、通知与确认弹窗。
- 设置持久化（图库列表、AI 参数、性能参数、界面参数），原子写入与损坏备份。
- 图库清单 `.artbank/library.json`、SQLite schema v1、单写 + 多读连接池、WAL/FTS5。
- 图库创建/打开/关闭/移除/重命名、状态查询、窗口控制命令。
- 浏览器预览 mock：无 Tauri 时可用界面创建/打开演示图库，不写磁盘。

### S1 性能原型 + 导入管线

- WebP 缩略图编码（最长边 512、quality 80），`webp` 0.3 + `libwebp-sys` 0.9.6 静态编译。
- 追加式缩略图包 `0001.pack`、16 字节段头、1 GiB/段、串行追加、句柄缓存与定位读。
- `thumb://` 自定义协议（Windows 为 `http://thumb.localhost`），长缓存、ETag、404/500 语义。
- 文件/目录/拖放导入、流式 BLAKE3、临时副本 + 原子发布、EXIF 方向、解码上限、pHash、4/8 主色、精确/批内/感知重复、rayon 工作池、单写批量入库、进度/取消/失败清单。
- 导入界面、拖放覆盖层、结果预览。1000 张合成图实测 47.7–50.0 张/秒。

### S2 虚拟化网格、查询与选择

Rust：

- `library/query.rs`：`LibraryQuery`、键集游标、视图（all/trash）、文件夹/扩展名/评分/标签（any/all）筛选、导入时间/评分/大小排序、分页（默认 150，最多 300）。
- 命令：`query_images`、`query_image_ids`（当前筛选全部 id）、`get_filter_counts`（四个分面，排除自身筛选）、`get_folders`、`get_tags`。
- 返回行含缩略图版本与尺寸、标签摘要、原图绝对路径；不含 embedding/pose。
- 标签摘要在页内批量查询；游标用 `(sort_col, id) < (?)` 行值比较，不使用深 `OFFSET`。
- `platform/clipboard.rs` + `copy_text` 命令：`arboard` 写系统剪贴板，供右键“复制文件路径”使用。
- 查询单测覆盖分页无重复、重复排序键、筛选、升降序、回收站视图、计数、列表、全部 id、非法参数。

前端：

- `src/modules/library/`：types、api、model、`useImageQuery`、`LibraryView`、`VirtualGrid`、`VirtualList`、`FilterPanel`、`LibraryThumbnail`。
- 自适应列数虚拟网格与固定行高虚拟列表：只挂载视口 + 前后一屏；缩略图失败显示占位并可重试。
- 基础筛选面板：文件夹、扩展名、评分、标签（任一/全部）、条件计数、清除；工具栏排序与网格/列表切换。
- 选择：单击、Ctrl/Cmd、Shift 区间、`Ctrl+A`（Rust 返回全部匹配 id）；方向键/PageUp/PageDown/Home/End；Enter/双击提示查看器占位；Delete 提示 S3。
- 右键菜单：打开查看器（占位）、复制文件路径、选择全部匹配项。
- 请求代次丢弃过期页；首屏/翻页错误可重试；筛选变化清空选择，排序变化保留选择；导入完成刷新第一页。
- 切换图库时以图库 id 重新挂载视图；浏览器预览使用 4,000 行 mock 与 data URL 缩略图。
- 未改 schema；不保存原始文件名，列表只显示尺寸、格式、大小、评分、标签、时间与文件夹。

## 验证结果

已运行并通过：

```text
npm run typecheck
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets   # 额外检查：无警告
cargo test --manifest-path src-tauri/Cargo.toml --lib   # 49 passed, 4 ignored
git diff --check                                        # 无空白问题
```

20 万行查询引擎（`perf_metadata_200k`，开发档，4 读连接）：

```text
first page 150 (imported desc)   p50 0.62ms / p95 0.79ms
deep keyset 150 (score/id)       p50 1.08ms / p95 1.24ms
folder 150                       p50 0.58ms / p95 0.83ms
tag 150                          p50 2.91ms / p95 3.16ms
filter counts                    p50 27.36ms / p95 40.32ms
select-all 200k ids             17.61ms
```

全部低于预算（首屏/深分页/文件夹/标签 ≤50ms，计数 ≤100ms）。

浏览器预览（Chromium 代理，724×712 CSS，4,000 行 mock）：

- 150 行时同时挂载 12–24 个网格单元格；300 行时约 18 个；列表约 24 行。
- 滚动到底继续加载下一页；筛选/排序后回到第一页，无旧行残留。
- 文件夹/评分筛选、空结果、清除筛选、选择、Ctrl+A、右键菜单、缩略图失败重试均通过。
- 控制台无 error/warn。
- 该检查不代表 WebView2 性能；真实窗口仍需手工验收。

debug exe 的 PE import 仍只有系统 DLL（新增剪贴板依赖未引入额外 DLL）。

## 未验证

- 真实 Tauri 窗口的 S1 导入界面、拖放、进度/取消与结果预览。
- 真实 Tauri 窗口的 S2 网格/列表：20 万行滚动帧率、内存、150 张缩略图从请求到可见、1440 宽度下的列布局。
- 真实窗口的筛选面板、键盘导航、Shift/Ctrl 选择、右键“复制文件路径”（arboard）、缩略图 404 重试。
- 20 万 id 全选经 Tauri IPC/JSON 到前端的耗时与内存。
- 真实 WebView2 的缩略图冷/热耗时与缓存行为。
- 真实美术图样本（大图、透明图、EXIF 方向、不同压缩率）的导入与显示。
- 长时间滚动到全库末尾时前端已加载页的内存增长（当前不回收旧页）。
- release 构建的 exe 与 DLL 清单（S9）。
- 真实 Tauri 窗口的启动、窗口拖动/最小化/最大化/关闭、设置持久化（S0 遗留）。

## 下一步

1. 用户手工验收：`npm run tauri dev` → 打开图库 → 导入混合格式样本 → 关闭导入弹窗 → 在网格/列表中滚动、筛选、选择、右键复制路径；再用 20 万级库观察滚动与首屏。
2. 验收通过后进入 S3：元数据编辑、搜索/完整筛选、标签/文件夹管理、集合与回收站。
3. S3 开始时补上搜索框、时间/大小范围、列表列头排序与回收站入口；S4 接入查看器与复制图片。

## 约束

- `G:\Lilith Image Library` 未修改；所有测试、基准与样本使用临时目录。
- 不读旧 `.data`；迁移脚本不在本仓库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏。

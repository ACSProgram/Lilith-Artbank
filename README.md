# Lilith Artbank

面向数十万张图片、数百 GB 素材的本地美术图库。Tauri 2 + Rust 负责元数据、文件与图像管线，React 只做视图与交互。

本项目是内部工具，不做公开版本发布，因此没有版本节奏、CHANGELOG 与法律许可文件。所有实现以开发效率和运行性能为先。

## 当前状态

**Stage 0 已完成**：项目骨架、设置持久化、图库创建/打开/关闭、SQLite schema v1、应用外壳与图库入口。

尚未实现：图片导入、缩略图、网格浏览、元数据编辑、筛选排序、查看器、图像处理、AI 服务调用。路线见 [docs/planning/roadmap.md](docs/planning/roadmap.md)。

## 功能边界

保留原 Lilith 图库本体的能力：导入与去重、标签/评分/描述、文件夹、筛选排序、集合、回收站、查看器、裁剪/旋转/调尺寸、图像处理（色彩/LUT/滤波/卷积）、批量维护。

明确不做：

- PinBoard 画板、ImagePaste 贴图、PasteBoard 截屏附件；
- OKLCH 调色盘、色彩测试等绘画小工具；
- 旧图库格式的读取与数据迁移（后续单独做一次性脚本，不在应用内）；
- 公开版本号、CHANGELOG、LICENSE 等发布材料。

AI 能力（description / embedding / pose）统一走本地 HTTP 服务，应用负责托管服务进程；服务器实现与 Python 环境不在本仓库。

## 技术栈

| 层 | 选择 |
| --- | --- |
| 桌面壳 | Tauri 2（Windows，WebView2 系统运行时，不随包附带 DLL） |
| 前端 | React 19 + TypeScript + Vite 6，手写 CSS，无 UI 框架 |
| 元数据 | SQLite（`rusqlite` bundled，静态编译），WAL + FTS5 |
| 图像 | `image` crate；缩略图为追加式 `.pack`，浏览器负责解码 |
| 并发 | 单写连接 + 多读连接；`rayon` 处理导入与图像计算 |
| AI | 本地 HTTP `/v1` 接口；应用托管进程，接口见 [docs/modules/ai-service.md](docs/modules/ai-service.md) |

## 数据布局

```text
<library>/
  .artbank/
    library.json         图库清单
    library.db           SQLite 元数据（WAL）
    thumbs/0001.pack     追加式缩略图包
    features/            embedding / pose 包（Stage 6）
    temp/
  <folder>/<content-hash>.<ext>
  recycle/
  output/
```

原图按 BLAKE3 内容哈希命名，内容不变就不动文件；编辑产生新哈希文件，数据库用稳定的图片 id 引用标签与集合。

## 性能基线（本机实测）

原始 C++ 版本的目标是“一页 150 张缩略图 1 秒内加载并渲染”。本机（Windows 11，开发档，Chromium 150）初步实测：

| 项目 | 结果 |
| --- | --- |
| SQLite 20 万行首屏 150 条（按评分） | p50 0.12 ms |
| 深分页 150 条（键集游标） | p50 0.53 ms |
| 按文件夹取 150 条 | p50 0.07 ms |
| 按标签取 150 条 | p50 1.95 ms |
| FTS 三字中文查询 | p50 0.48 ms |
| 200k 元数据 + 600k 标签关系写入 | 7.01 s |
| 150×20KB 随机读：缩略图包 vs 逐文件 | 0.60 ms vs 9.69 ms |
| 150 张 512px JPEG：冷加载 / 热缓存 | 456 ms / 236 ms |
| 150 张 512px WebP：冷加载 | 437 ms，体积约为 JPEG 一半 |

结论：SQLite 不是瓶颈；缩略图包明显优于逐文件；浏览器解码满足 1/3 效率门槛。完整方法与结论见 [docs/guides/performance.md](docs/guides/performance.md)。

## 开发

```bash
npm install
npm run tauri dev       # 真实 Tauri 窗口
npm run dev             # 浏览器预览（mock 数据，不写磁盘）
npm run typecheck
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

性能基准默认忽略，按需运行：

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_metadata_200k
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_thumb_pack_read
```

`G:\Lilith Image Library` 下的真实仓库只允许只读查看；任何测试、基准与开发流程都不得修改它。

## 文档

从 [docs/README.md](docs/README.md) 进入；代码任务先读 [docs/architecture/ai-reading-guide.md](docs/architecture/ai-reading-guide.md)。

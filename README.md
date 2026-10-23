# Lilith Artbank

面向数十万张图片、数百 GB 素材的本地美术图库。Tauri 2 + Rust 负责元数据、文件与图像管线，React 只做视图与交互。

本项目是内部工具，不做公开版本发布，因此没有版本节奏、CHANGELOG 与法律许可文件。所有实现以开发效率和运行性能为先。

## 当前状态

**S0 已完成**：项目骨架、设置持久化、图库创建/打开/关闭、SQLite schema v1、应用外壳与图库入口。

**S1 已完成（自动验收通过）**：512px WebP 缩略图编码（libwebp 静态链接）、追加式缩略图包、`thumb://` 自定义协议、应用内 150 张加载基准；导入管线（文件/目录/拖放、BLAKE3、EXIF 方向、pHash、主色、重复策略、进度/取消/失败清单）与导入界面。

**S2 已实现（自动验收通过）**：键集分页查询引擎（视图、文件夹/扩展名/评分/标签筛选、时间/评分/大小排序）、筛选分面计数、文件夹与标签列表、全部匹配 id、系统剪贴板文本；前端自适应虚拟网格与固定行高列表、缩略图占位与重试、选择/Ctrl+A/键盘导航/右键菜单、基础筛选面板。

**S3 已实现（自动验收通过）**：描述/标签名/文件夹路径搜索（当前视图或全库）、导入时间与文件大小范围、描述/embedding/pose 存在性筛选、集合内手工顺序；详情与批量编辑（评分/描述/标签/文件夹移动）、标签重命名与删除、集合 CRUD/排序/并集交集差集、回收站软删除/恢复/永久删除/清空。普通视图的分面计数在预算内，搜索与超大集合的计数超 100 ms 时筛选面板显示加载态。

**S4 已实现（自动验收通过）**：全屏查看器（缩放/平移/上下张/键盘）、大图与 EXIF 方向图的 Rust WebP 预览、旋转/翻转/裁剪/调尺寸、JPEG/PNG/WebP 输出、新内容发布式保存、`img://` 原图与预览协议、复制图片到剪贴板、可编辑详情侧栏。

**S6 已实现（自动验收通过）**：本机 `/v1` AI 客户端与服务进程托管、feature pack（embedding fp16 / pose f32）、打开图库计算全库均值、mmap 中心化余弦语义搜索、embedding/pose/description 批量任务（进度/取消/失败清单）、查看器姿势生成与叠加、详情侧栏 AI 描述。

S5 图像处理（色彩/LUT/DFT/卷积）已按用户决定取消。真实 WebView2 的 150 张门槛已由用户确认；真实窗口中的导入、浏览、编辑、集合、回收站、查看器、AI 服务与语义搜索待手工验收。尚未实现：S7 的颜色/pHash/向量/pose 相似度与高级集合。路线见 [docs/planning/roadmap.md](docs/planning/roadmap.md)。

## 功能边界

保留原 Lilith 图库本体的能力：导入与去重、标签/评分/描述、文件夹、筛选排序、集合、回收站、查看器、裁剪/旋转/调尺寸、批量维护。图像处理（色彩/LUT/滤波/卷积）已按用户决定取消，不再实现。

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
| 图像 | `image` crate + 静态 libwebp；缩略图为追加式 `.pack`，浏览器负责解码 |
| 并发 | 单写连接 + 多读连接；`rayon` 处理导入与图像计算 |
| AI | 本地 HTTP `/v1` 接口；应用托管进程，接口见 [docs/modules/ai-service.md](docs/modules/ai-service.md) |

## 数据布局

```text
<library>/
  .artbank/
    library.json         图库清单
    library.db           SQLite 元数据（WAL）
    thumbs/0001.pack     追加式缩略图包
    features/            embedding（fp16）/ pose（f32）追加式包
    temp/                原子写入与导入临时目录；启动时清理
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
| 查询引擎首屏 150 条（含 JOIN 与标签摘要） | p50 0.62 ms |
| 查询引擎筛选分面计数（文件夹/扩展名/评分/标签） | p50 27.36 ms |
| 筛选分面计数（FTS 搜索，20 万行） | p50 132.56 ms |
| 筛选分面计数（20 万条集合） | p50 778.83 ms |
| 全选 20 万条 id | 17.61 ms |
| FTS 三字中文查询 | p50 0.48 ms |
| 200k 元数据 + 600k 标签关系写入 | 7.01 s |
| 150×20KB 随机读：缩略图包 vs 逐文件 | 0.60 ms vs 9.69 ms |
| 150 张 512px WebP 生成 + 写入 pack | 3.97 s，9.00 MB（开发档） |
| 同一 pack 内 150 张（约 9 MB）定位读 | p50 2.27 ms / p95 2.80 ms |
| 150 张 512px JPEG：冷加载 / 热缓存 | 456 ms / 236 ms |
| 150 张 512px WebP：冷加载 | 437 ms，体积约为 JPEG 一半 |
| 1000 张 768×512 混合格式导入（开发档，4 线程） | 约 20 s，50.0 张/秒 |

结论：SQLite 不是瓶颈；缩略图包明显优于逐文件；浏览器解码满足 1/3 效率门槛；导入吞吐高于 20 张/秒下限；S2/S3 的翻页查询（含搜索与集合）仍在毫秒级，分面计数在搜索与超大集合下超过 100 ms 预算，筛选面板以加载态呈现。完整方法与结论见 [docs/guides/performance.md](docs/guides/performance.md)。

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
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_import_1000
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_thumb_bench_build
```

真实 WebView2 的缩略图基准在开发构建中打开任意图库，点击工作区右上角“缩略图基准”。

`G:\Lilith Image Library` 下的真实仓库只允许只读查看；任何测试、基准与开发流程都不得修改它。

## 文档

从 [docs/README.md](docs/README.md) 进入；代码任务先读 [docs/architecture/ai-reading-guide.md](docs/architecture/ai-reading-guide.md)。

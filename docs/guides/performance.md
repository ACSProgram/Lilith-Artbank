# 性能基准

## 原始基线

原 C++ 图库的参考表现（用户描述）：

- 二进制元数据/缩略图数据加载吞吐约 100 MB/s 到 >1 GB/s（不含解压）；
- 一页 150 张缩略图加载并渲染在 1 秒内。

新程序的最低要求是达到原性能的 **1/3**：一页 150 张缩略图从请求到可见 ≤3s；目标仍是 ≤1s。元数据侧不要求复刻“整库载入吞吐”，因为新结构不整库载入；我们看首屏查询与翻页延迟。

## 方法

### 元数据：200k 行

`perf_metadata_200k`（临时 SQLite）：

- 200,000 张图片、20 个文件夹、200 个标签、600,000 条标签关系；
- 描述含中文，FTS5 trigram 索引；
- 评分/文件夹/标签/FTS 查询各取 150 条；
- 深分页用键集游标 `(score, id) < (?, ?)`；
- 记录 p50/p95、写入耗时、重开库耗时。

运行：

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_metadata_200k
```

### 缩略图包 vs 逐文件

`perf_thumb_pack_read`：

- 150 个 20KB blob；
- 一个追加式 pack（记录 offset）与 150 个独立文件；
- 80 轮随机顺序读取，取 p50；
- 操作系统缓存已预热，衡量的是应用层读取/打开开销。

运行：

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_thumb_pack_read
```

### 浏览器解码

`tools/perf/thumb_bench/`：

- Pillow 生成 150 张 512×512、含渐变/形状/噪声的图片；
- JPEG q82 与 WebP q80 各一份；
- 页面 15×10 网格，全部可见；`img.decode()` 全部完成后等两帧 rAF；
- 计时从 navigation start 到完成；记录传输体积、解码体积与资源数；
- 本地 HTTP 服务器模拟 Tauri 自定义协议的服务路径。

运行：

```bash
python tools/perf/thumb_bench/generate.py
node tools/perf/thumb_bench/server.mjs
# 浏览器打开 http://127.0.0.1:18767/?format=jpg 和 ?format=webp
```

### 应用内缩略图基准（S1 前半）

开发构建的工作区工具栏有“缩略图基准”。它用 Rust 生成 150 张 512px 确定性图片，经正式管线编码为 WebP q80、写入临时追加式 pack，然后让 WebView2 通过 `thumb://` 加载：

- 冷加载：每次使用新的 `?run=<n>`，浏览器缓存必然未命中；
- 热加载：复用同一 URL，测浏览器缓存路径；
- 计时从创建 `<img>` 到 150 张全部 `decode()` 完成后再等两帧 rAF；
- 同时显示 Rust 侧 pack 读取 p50/p95 与包体积。

```bash
npm run tauri dev
# 打开任意图库 → 工作区右上角“缩略图基准” → 生成样本 → 冷加载 / 热加载
```

真实 WebView2 的数字由用户在本机手工验收；用户已确认冷/热加载满足 ≤1s 门槛，具体数字尚未记录。浏览器预览不支持该基准。

### 导入吞吐（S1 后半）

`perf_import_1000`：

- 临时目录生成 1000 张 768×512 确定性图片，JPEG/PNG/BMP/WebP 各 25%；
- 建立临时图库，4 个工作线程；
- 计时覆盖扫描、BLAKE3、解码、EXIF、缩略图、pHash、主色、原子发布、pack 写入与批量入库，不含样本生成；
- 断言新增 1000、失败 0、吞吐 ≥20 张/秒。

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_import_1000
```

### 发布产物依赖

```bash
cargo build --manifest-path src-tauri/Cargo.toml
python tools/perf/pe_imports.py src-tauri/target/debug/lilith-artbank.exe
```

脚本列出 PE 的 import 与 delay-import DLL。当前结果只包含系统 DLL，没有 `WebView2Loader.dll`、`vcruntime140.dll` 或 `webp.dll`。release 产物的最终核对仍留在 S9。

## 实测记录（2026-10-23）

环境：Windows 11，Rust 1.97.1，开发档（依赖 opt-level 2），本机 SSD；浏览器为 Qoder 内置 Chromium 150（Electron 43）。这是 Chromium 代理，不是 WebView2；Stage 1 会在真实 Tauri 窗口复测。

### 元数据（200k）

| 项目 | 结果 |
| --- | --- |
| 写入 200k 图片 + 600k 标签关系（含 FTS/索引） | 7.01 s（约 28.5k 图片/s） |
| 首屏 150 条（评分降序） | p50 0.12 ms / p95 0.15 ms |
| 深分页 150 条（id≈150k） | p50 0.53 ms / p95 0.57 ms |
| 文件夹页 150 条 | p50 0.07 ms / p95 0.08 ms |
| 标签页 150 条 | p50 1.95 ms / p95 2.29 ms |
| FTS trigram 中文查询 | p50 0.48 ms / p95 0.65 ms |
| 活跃计数 | 5.12 ms |
| 文件夹分组计数（20 组） | 8.67 ms |
| 重开图库 + 计数 | 24.62 ms |

### 缩略图包

| 项目 | 结果 |
| --- | --- |
| 150×20KB 随机读，pack | p50 0.60 ms（约 4.9 GB/s，缓存命中） |
| 150×20KB 随机读，逐文件 | p50 9.69 ms（约 302 MB/s） |
| 优势 | 约 16× |

### 浏览器（150 张 512px）

| 格式 | 冷加载 | 热缓存 | 传输体积 | 结论 |
| --- | --- | --- | --- | --- |
| JPEG q82 | 456.3 ms | 235.5 ms | 2.91 MB | 满足 ≤1s |
| WebP q80 | 437.1 ms | — | 1.46 MB | 更快、体积约一半 |

单张平均：JPEG 19.6 KB，WebP 9.6 KB（合成图；真实美术图会更大，但相对关系一致）。

### 应用内 pack 与 WebP（S1 前半，2026-10-23）

同一台机器、Rust 1.97.1、开发档（本 crate opt-level 1，依赖 opt-level 2）：

| 项目 | 结果 |
| --- | --- |
| 150 张 512px 确定性样本：生成 + WebP q80 + 追加 pack | 3.97 s |
| 样本包体积 | 9.00 MB（平均 60 KB/张） |
| 同一 pack 内 150 张定位读，20 轮 | p50 2.27 ms / p95 2.80 ms（约 4 GB/s，OS 缓存命中） |
| debug exe 导入 DLL | 仅系统 DLL；无 libwebp / vcruntime / WebView2Loader |

前三项由 `perf_thumb_bench_build` 输出：

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_thumb_bench_build
```

WebView2 中的 150 张冷/热加载已由用户手工确认满足 ≤1s 门槛，具体耗时未记录。

### 导入（S1 后半，2026-10-23）

同一台机器、Rust 1.97.1、开发档（本 crate opt-level 1，依赖 opt-level 2），4 个工作线程：

| 项目 | 结果 |
| --- | --- |
| 1000 张 768×512 混合格式导入（含特征与缩略图） | 19.99–20.97 s，47.7–50.0 张/秒（两次） |
| 失败 / 重复 | 0 / 0 |
| 门槛 | ≥20 张/秒，达标 |

样本是合成图，真实美术图的解码与主色耗时会更高；S9 用复制出来的真实小样本复测。

## 结论与决策

1. **SQLite 不是瓶颈**。首屏、深分页、文件夹、FTS 都在毫秒级；不需要自研元数据索引。
2. **缩略图包优于逐文件**。随机读快约 16 倍，且避免 20 万小文件对备份、扫描和冷启动的压力。
3. **浏览器解码满足门槛**。150 张 512px 缩略图冷加载约 0.44-0.46s，远低于 3s 底线，也低于 1s 目标。
4. **WebP 已定稿**。`webp` 0.3 + `libwebp-sys` 0.9.6 静态编译，构建可接受，debug exe 无额外 DLL；缩略图统一 WebP q80。
5. **不需要 DDS/WebGPU 缩略图管线**。浏览器解码已达标，自绘网格的复杂度与风险不值得。
6. **导入吞吐达标**。1000 张合成图 47.7–50.0 张/秒，高于 20 张/秒的阶段下限；瓶颈仍在解码、WebP 编码与 k-means，而不是 SQLite 写入。

## 仍需复测

- 真实 Tauri WebView2 的缩略图冷/热具体耗时与 WebView2 缓存行为；
- 冷磁盘缓存（清空系统缓存后的首次读取）；
- 真实美术缩略图（平均 30-80KB）与 512px 下 20 万张的包分段；
- 真实美术图导入（大图、透明图、EXIF 方向、不同压缩率）与吞吐；
- CLIP/pose 全库扫描耗时与内存；
- 前端在 20 万行下的滚动帧率与内存。

这些在对应阶段执行，结果追写本文件。

## 性能预算

| 路径 | 预算 |
| --- | --- |
| 打开图库 | ≤1s（不含首次迁移） |
| 首屏 150 条元数据 | ≤50ms |
| 深分页 | ≤50ms |
| 一页 150 张缩略图可见 | ≤1s（硬上限 3s） |
| 筛选面板计数 | ≤100ms；更慢则显示加载态 |
| 语义搜索（200k） | 首版 ≤500ms |
| 编辑保存（4K） | ≤2s（DFT 可更慢） |
| 前端滚动 | 60fps 目标；不阻塞主线程 |

任何阶段若引入超预算路径，先停下优化或重新设计，不要用“以后再优化”掩盖。

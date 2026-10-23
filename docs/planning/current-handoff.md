# 当前阶段状态

日期：2026-10-23
阶段：**S1 已完成（自动验收通过）；真实窗口的导入界面与拖放待用户手工验收**

## 已完成

### S0 工程骨架

- git 仓库、Tauri 2 + React 19 + TypeScript + Vite 6 工程。
- 无边框自绘标题栏、Lilith Artworks 风格浅色应用外壳、侧栏、欢迎页、设置弹窗、新建/打开图库弹窗、通知与确认弹窗。
- 设置持久化（图库列表、AI 参数、性能参数、界面参数），原子写入与损坏备份。
- 图库清单 `.artbank/library.json`、SQLite schema v1、单写 + 多读连接池、WAL/FTS5。
- 图库创建/打开/关闭/移除/重命名、状态查询、窗口控制命令。
- 浏览器预览 mock：无 Tauri 时可用界面创建/打开演示图库，不写磁盘。

### S1 前半：性能原型

- WebP 缩略图编码：`imageops::thumbnail`，最长边 512、quality 80；`webp` 0.3 + `libwebp-sys` 0.9.6 静态编译。
- 追加式缩略图包：`library::thumbs::ThumbPack`，`0001.pack` 起、16 字节段头、默认 1 GiB/段、串行追加、句柄缓存与定位读、指针越界校验。
- 图库会话持有 `Arc<ThumbPack>`；打开图库只扫描 pack 文件，不载入全量元数据。
- `thumb://` 自定义协议：按图片 id 读 DB 指针并从 pack 返回字节；带 MIME、长缓存、ETag；Windows 上为 `http://thumb.localhost`。pack 缺失或指针失效返回 404。
- 开发基准：Rust 生成 150 张 512px 样本写入临时 pack，工作区“缩略图基准”全屏页测量 WebView2 冷/热加载；另提供 Rust 侧 pack 读取 p50/p95。
- 视觉统一：整个应用改用 Lilith Artworks 的浅色绿系语言（暖白底、白面板、深绿强调、6/8px 圆角），基准页与导入页同步。
- 构建辅助：`tools/perf/pe_imports.py` 列出 PE import / delay-import DLL。

### S1 后半：导入管线

- 输入：文件/目录/拖放；递归扫描跳过 `.artbank`、图库内的 `recycle`/`output` 与符号链接；单次最多 10000 个输入路径、100000 个文件。
- 流式 BLAKE3 + `.artbank/temp/import-<job>/` 临时副本 + `sync_all` + `rename` 原子发布；目标已存在时复用。
- 解码：使用 `image` crate 的 EXIF 方向；单边 65535、1 亿像素、512 MiB 分配上限；支持 JPEG/PNG/BMP/WebP/TIFF/TGA/GIF（首帧）。
- 特征：`imageops::phash`（256 位，DC 排除、0.5 量化阈值）与 `imageops::colors`（OKLab 固定种子 k-means，4/8 色，权重合计 255）。
- 重复：精确重复按内容哈希跳过或合并（标签并集、评分取高）；批内重复只保留一份；感知重复用内存 pHash 线性扫描，默认阈值 6，只提示。
- 入库：rayon 工作池（线程数取设置）、单写通道、64 张/250ms 批量事务、缩略图追加 pack、`thumb_segments` 行与段尺寸维护。
- 任务：job id、150ms 进度事件、取消、失败清单（最多 200 条）、疑似重复提示（最多 100 条）、最近 24 张预览 id；导入期间禁止关闭/切换图库。
- 界面：工作区“导入图片”、拖放覆盖层、导入选项、进度、取消、结果统计、缩略图预览与失败详情。
- 浏览器预览不执行导入，提示需要 Tauri 桌面窗口。

## 验证结果

已运行并通过：

```text
npm run typecheck
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml   # 42 passed, 4 ignored
git diff --check
```

导入吞吐（开发档，4 线程，1000 张 768×512 混合 JPEG/PNG/BMP/WebP）：

```text
imported=1000 failed=0 duplicates=0 elapsed=19.99s rate=50.0/s
# 另一次运行：20.97s / 47.7 张/秒
```

门槛为 ≥20 张/秒，已达标。方法与限制见 [性能基准](../guides/performance.md)。

自动测试覆盖：

- 混合格式入库；缩略图指针、pHash、主色长度完整；原图文件存在；
- EXIF 方向 6 的 JPEG 解码后宽高交换，方向 1 保持原尺寸；
- 同一目录二次导入全部计为重复；合并策略标签并集与评分取高；
- 取消后数据库行数与已提交一致、临时目录为空；
- 删除 pack 后读取返回错误而不是 panic；
- 路径/标签/评分/文件夹校验。

S1 前半的 150 张缩略图门槛已由用户在真实 Tauri 窗口手工确认满足 ≤1s；具体冷/热数字未记录。

## 未验证

- 真实 Tauri 窗口中的导入界面、拖放、进度/取消与结果预览（用户手工验收）。
- 真实美术图样本（大图、透明图、EXIF 方向、不同压缩率）的解码、方向与吞吐。
- 真实 WebView2 的 150 张冷/热具体耗时与 `Cache-Control: immutable` 缓存行为。
- 真实 Tauri 窗口的启动、窗口拖动/最小化/最大化/关闭、设置持久化（S0 遗留）。
- release 构建的 exe 与 DLL 清单（S9 再核对）。
- 真实美术缩略图体积与 1 GiB/段在 20 万张规模下的段数/句柄。

## 下一步

1. 用户手工验收：`npm run tauri dev` → 打开图库 → “导入图片”或拖入混合格式样本 → 观察进度、取消与结果；再确认关闭图库在导入期间被拒绝。
2. 验收通过后进入 S2：查询引擎（键集游标）、虚拟化网格/列表、缩略图占位与失败重试、选择与键盘操作。
3. S2 开始时把导入完成后的预览替换为真正的网格视图。

## 约束

- `G:\Lilith Image Library` 未修改；所有测试、基准与样本使用临时目录。
- 不读旧 `.data`；迁移脚本不在本仓库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏。

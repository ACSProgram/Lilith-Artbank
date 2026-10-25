# 导入与缩略图

导入把外部图片变成图库中的“原图 + 元数据 + 缩略图指针”。S1 前半完成缩略图编码、追加式 pack 与 `thumb://` 协议；S1 后半完成导入管线、pHash、主色、重复策略、后台任务与导入界面。

## 当前实现

### 输入与校验

- 文件选择器：多选图片；目录选择器：递归扫描。
- 拖放：Tauri 原生 `drag-drop` 事件；窗口任意位置可放下图片或目录。
- 支持格式：JPEG、PNG、BMP、WebP、TIFF、TGA、GIF（首帧）。HEIC/AVIF 暂不支持。
- 扫描跳过 `.artbank` 与旧程序的 `.data`；若扫描范围在图库内，同时跳过 `recycle` 与 `output`；不跟随符号链接。
- 单次最多 10000 个输入路径、最多收集 100000 个文件；超出时停止扫描并在失败清单记录。
- 导入选项：目标文件夹（图库内相对路径）、目录层级模式（平铺 / 保留源目录）、初始标签（最多 32 个，单个 ≤64 字符）、初始评分 0-5、重复策略（跳过 / 合并元数据）。
- 路径必须是绝对路径；目标文件夹经 `normalize_folder` 校验，拒绝 `..`、盘符与绝对路径。
- 目录扫描为每个文件记录相对于“每个输入目录根”的 `relative_folder`；直接选中的文件该值为空。
- `folderMode=flat`：所有图片进入目标文件夹；`folderMode=preserve`：目标文件夹 + 源相对目录拼接后作为最终文件夹，空相对目录仍进入目标文件夹。所有不存在的祖先由导入写线程自动补齐。
- 浏览器预览不执行导入；界面会提示需要 Tauri 桌面窗口。

### 单张图片管线

```text
读取文件 → 流式 BLAKE3 + 临时副本（1 MiB 缓冲，sync_all）
  → 查 content_hash / 批内集合（精确重复）
  → 解码（EXIF 方向、尺寸与分配上限）
  → 记录宽高 / 格式 / alpha
  → 512px WebP 缩略图
  → 256 位 pHash
  → 4 色 / 8 色主色
  → 原子发布原图 <folder>/<hash>.<ext>
  → 结果送入单写通道，批量入库
```

- 临时文件位于 `.artbank/temp/import-<jobId>/`，保留源扩展名：格式先按魔数判断，魔数不认识的格式（TGA）再用扩展名兜底；成功发布后目录整体清理。
- 原图按 `<content_hash>.<ext>` 命名；扩展名取自解码后的格式（`jpeg → jpg`，其余与格式名一致），不跟源文件名，避免改名文件的扩展名与 `format` 列矛盾。
- 目标文件已存在（通常是上次崩溃留下的同内容文件）时复用，不覆盖；发布用 `rename`，不跨卷。
- 解码上限：单边 65535、总像素 1 亿、解码分配 512 MiB。超限进入失败清单。
- EXIF 方向由 `image` crate 的 `ImageDecoder::orientation()` 读取并 `apply_orientation`，只影响缩略图、pHash、主色与后续显示，不改原图字节。

### pHash（256 位）

1. 转灰度并缩放到 32×32。
2. 做二维 DCT-II，取左上 16×16 低频系数。
3. DC 分量不参与中位数；绝对值小于 0.5 的系数归零，避免纯色图的浮点噪声随机点亮一半的位。
4. 按其余系数中位数二值化，得到 256 bit，存 32 字节 BLOB。

用途是相似度与疑似重复提示，不参与主键。相似度扫描用 4 个 `u64` 做异或+popcount。

### 主色 4 / 8

1. 缩略到 64×64（Triangle）。
2. 转 OKLab，使用固定种子的 k-means++ 初始化，最多 12 次迭代。
3. 每簇存 `(r, g, b, weight)` 各 1 字节；权重按最大余数法合计 255。
4. 4 色 16 字节、8 色 32 字节；按权重降序。

不追求与旧 OpenCV EMD 逐值一致；目标是稳定、可解释、支持加权颜色距离。透明图的主色与 pHash 都按解码后的 RGB 计算，不额外合成背景；缩略图保留 alpha。

### 重复策略

- **精确重复**：`content_hash` 已存在。默认跳过并计入“重复跳过”；选择“合并元数据”时标签取并集、评分取较高值、更新 `modified_at`。
- **批内重复**：HashSet 在发布前占位，同一批内相同内容只保留一份。批内重复即使选择合并也按跳过处理，避免写入顺序不确定。
- **感知重复**：导入时把新 pHash 与库内 pHash 线性比较（默认汉明距离阈值 6/256），只记录提示，不自动跳过。任务开始时一次性载入库内 pHash 的 4×`u64` 紧凑数组；新入库图片在提交后追加。20 万规模约 6.4 MB 原始数据，S7 再加分桶/索引。
- 感知提示最多保留 100 条；结果里显示 `imageId / similarTo / distance`。
- 回收站中的同内容图片仍视为已存在；导入不会自动恢复。S3 已提供回收站界面，可在导入后手工恢复。

### 缩略图包与指针

- 缩略图最长边 512px、WebP quality 80；透明图走 RGBA。
- 追加式 `.pack`：`0001.pack` 起、16 字节段头（`ARTBTHM1` + u32 版本 + 4 字节保留）、默认 1 GiB/段。
- 写入由单写通道串行执行，每个 blob 后 flush；读取按 segment 缓存句柄并定位读，单 blob 上限 32 MiB。
- pack 不做逐 blob fsync。进程崩溃只会留下未引用 blob；断电后指针可能失效，按缓存缺失处理，由维护任务重建。
- 每批事务内写入 `thumb_segment / thumb_offset / thumb_length / thumb_format / thumb_width / thumb_height / thumb_version = 1`，同时维护 `thumb_segments` 行与段文件尺寸。
- 崩溃最多留下未引用的 blob；已提交行不受影响。pack 丢失或指针失效时协议返回 404，由前端显示占位，不会崩溃。

### `thumb://` 协议

- URL：`thumb://<imageId>?v=<thumbVersion>`。Windows 的 WebView2 只接受 `http://thumb.localhost/<imageId>?v=<thumbVersion>`；前端统一经 `src/shared/thumbUrl.ts` 的 `thumbnailUrl()` 生成，不自己拼字符串。
- 查询串只作为浏览器缓存键，服务端只解析路径里的 id，不校验 `v`。
- 响应头：`Content-Type` 取自 `thumb_format`（当前只有 `image/webp`）、`Content-Length`、`Cache-Control: public, max-age=31536000, immutable`、`ETag: "v<thumb_version>"`、`X-Content-Type-Options: nosniff`。
- 状态码：id 不是整数 → 400；图库未打开、图片无缩略图、pack 缺失或指针越界 → 404；数据库与文件读取异常 → 500。错误响应带 `Cache-Control: no-store`，不会被长缓存钉住。网格/列表在图库打开后按 `thumb_version` 请求；404 时显示占位，点击重试会附加 `retry` 查询参数。

### 并发、进度与取消

- 像素工作走 rayon 工作池；线程数取设置里的 `workerThreads`，为空时使用 CPU 并行度 - 1。
- 结果通过有界通道送入唯一写线程；每 64 张或 250ms 批量提交；重复项最多攒 128 条。
- 每张图的阶段：读取/哈希 → 解码/特征 → 发布 → 入库。界面显示当前文件名与总进度。
- `import-progress` 事件每 150ms 推送一次；`import-finished` 在结束时推送完整快照。导入同时挂在任务总线上，右下角任务中心也能看到它与取消入口。
- 取消：工作线程在文件边界检查标记；已入库图片保留，未完成临时文件清理；写线程继续把已准备好的批次落库。
- 同一时间只允许一个导入任务；导入运行期间禁止关闭、创建或切换图库。
- 写线程按文件夹路径缓存 `folder_id`，同一批次重复进入同一目录时只解析/插入一次。
- 失败清单最多保留 200 条；每条包含路径与原因。
- 结果快照保留最近 24 个新增图片 id，用于导入完成后的缩略图预览。
- 内存峰值约为工作线程数 × 单图解码内存；默认 CPU 并行度 - 1，超大图多张并发时可在设置里降低 `workerThreads`。

## 前端契约

- 命令入口：`src/modules/import/api.ts`。
- 类型：`src/modules/import/types.ts`。
- 界面：`src/modules/import/ImportDialog.tsx`，由 `src/app/LibraryWorkspace.tsx` 打开；拖放监听在工作区，弹窗打开时由弹窗接管新增路径。目标文件夹使用与侧栏相同的 `FolderPickerDialog`，打开时预填当前浏览的单选文件夹（多选或根目录时为空）；目录层级用“平铺 / 保留源目录”两个按钮切换。
- 导入完成时工作区递增浏览刷新令牌，`LibraryView` 重新查询第一页并刷新图库计数；筛选条件保留，图片选择清空。结果弹窗仍显示最近 24 张预览，关闭后回到网格。

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `start_import` | `{ request }` | `ImportSnapshot`（立即返回，任务在后台执行） |
| `cancel_import` | `{ jobId }` | `null` |
| `get_import_status` | `{ jobId }` | `ImportSnapshot \| null` |

`ImportRequest`：

```text
paths: string[]            绝对路径，文件或目录
folder: string             目标文件夹，图库内相对路径，根目录为 ""
folderMode: "flat" | "preserve"  目录层级；默认 flat
tags: string[]             初始标签
score: number              0-5
duplicatePolicy: "skip" | "merge"
perceptualThreshold: number 默认 6，后端限制在 0-64
```

`workerThreads` 不由前端传入：`start_import` 一律用设置里的 `performance.workerThreads` 覆盖请求值，为空时取 CPU 并行度 - 1（上限 128）。

`ImportProgress`（camelCase）：`jobId`、`phase`（`scanning` / `processing` / `finishing` / `completed` / `cancelled` / `failed`）、`total`、`processed`、`imported`、`duplicates`、`merged`、`failed`、`similar`、`current`、`elapsedMs`。`ImportSnapshot` 另含 `startedAt`、`finishedAt`、`failures`、`hints`、`previewIds`。

事件名：`import-progress`、`import-finished`。

## 验收结果（2026-10-23）

自动测试覆盖并通过：

- 混合 JPEG/PNG/BMP/WebP 导入：8 张全部入库，缩略图指针、pHash 与主色长度完整，原图文件存在；
- EXIF 方向 6 的 JPEG 解码后宽高交换，方向 1 保持原尺寸；
- 再次导入同一目录：8 张全部计为重复，新增 0；
- TGA 通过扩展名兜底解码（魔数表里没有 TGA），得到正确格式与尺寸；
- 合并策略：标签并集、评分取较高值；
- 目录模式：`flat` 全部进入目标文件夹；`preserve` 按源相对目录建立多级文件夹，多个输入目录的同名子目录合并，祖先自动补齐；
- 取消：已提交行数与数据库一致，临时目录为空；
- 删除 pack：指针仍可读取，读取返回错误而不是 panic；
- 1000 张 768×512 混合格式（4 线程）：`imported=1000, failed=0, elapsed=19.99s, 50.0 张/秒`（复跑 20.97s / 47.7 张/秒），高于 20 张/秒门槛。

运行吞吐基准：

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_import_1000
```

## 尚未实现

- 剪贴板图片/路径输入。
- HEIC/AVIF 解码。
- 移动模式；默认只复制，不改用户原文件。源目录若在图库内，原文件也不会被移动或删除；请优先从图库外导入。
- 感知重复的分桶索引与阈值标定（S7 用真实样本做）。

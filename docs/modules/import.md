# 导入与缩略图

导入把外部图片变成图库中的“原图 + 元数据 + 缩略图指针”。S1 前半完成缩略图编码、追加式 pack 与 `thumb://` 协议；S1 后半完成导入管线、pHash、主色、重复策略、后台任务与导入界面。

## 当前实现

### 输入与校验

- 文件选择器：多选图片；目录选择器：递归扫描。
- 拖放：Tauri 原生 `drag-drop` 事件；窗口任意位置可放下图片或目录。
- 支持格式：JPEG、PNG、BMP、WebP、TIFF、TGA、GIF（首帧）。HEIC/AVIF 暂不支持。
- 扫描跳过 `.artbank`、旧程序的 `.data` 与临时文件夹目录（`.artbank-temp*`）；若扫描范围在图库内，同时跳过 `recycle` 与 `output`；不跟随符号链接。
- 单次最多 10000 个输入路径、最多收集 100000 个文件；超出时停止扫描并在失败清单记录。
- 导入选项：目标（图库 / 临时文件夹）、目标文件夹（图库内相对路径）、目录层级模式（平铺 / 保留源目录）、初始标签（最多 32 个，单个 ≤64 字符）、初始评分 0-5、重复策略（跳过 / 合并元数据）。
- 路径必须是绝对路径；目标文件夹经 `normalize_folder` 校验，拒绝 `..`、盘符与绝对路径。
- 目录扫描为每个文件记录相对于“每个输入目录根”的 `relative_folder`；直接选中的文件该值为空。
- `folderMode=flat`：所有图片进入目标文件夹；`folderMode=preserve`：目标文件夹 + 源相对目录拼接后作为最终文件夹，空相对目录仍进入目标文件夹。所有不存在的祖先由导入写线程自动补齐。
- 浏览器预览不执行导入；界面会提示需要 Tauri 桌面窗口。

### 导入到临时文件夹

- `ImportRequest.target=temp` 时不写操作记录，也不生成 `origin_operation_id`；导入不进入撤销/重做。
- 临时导入固定平铺、只允许跳过重复、不压缩；显式传 `preserve` 或 `merge` 会被拒绝，目标文件夹字段被忽略。界面隐藏目标文件夹、目录层级与压缩选项，只保留评分、标签与“跳过重复”。
- 图片直接写入图库根目录下的临时文件夹（`folders.kind='temp'`，通常是 `.artbank-temp`），不会进入普通视图、文件夹树或全库搜索。
- 关闭、切换图库或退出应用时清空临时文件夹；导入完成页与图库视图的提示都会说明这一点。
- MCP 的 `import_start(target=temp)` 在询问档不需要用户确认；图库导入仍然需要。

### 单张图片管线

```text
读取文件 → 流式 BLAKE3 + 临时副本（1 MiB 缓冲，sync_all）
  → 查 content_hash / 批内集合（精确重复）
  → 解码（EXIF 方向、尺寸与分配上限）
  → 记录宽高 / 格式 / alpha
  → 512px WebP 缩略图
  → 256 位 pHash
  → 4 色 / 8 色主色
  → 可选：导入时压缩（阶梯编码与 PSNR 裁决）
  → 原子发布原图或压缩结果 <folder>/<hash>.<ext>
  → 结果送入单写通道，批量入库
```

- 临时文件位于 `.artbank/temp/import-<jobId>/`，保留源扩展名：格式先按魔数判断，魔数不认识的格式（TGA）再用扩展名兜底；成功发布后目录整体清理。
- 原图按 `<content_hash>.<ext>` 命名；扩展名取自解码后的格式（`jpeg → jpg`，其余与格式名一致），不跟源文件名，避免改名文件的扩展名与 `format` 列矛盾。
- 目标文件已存在时不直接当成“内容重复”：rename 后、入库前崩溃会留下没有任何行引用的孤儿，文件可能被截断。先按 BLAKE3 核对目标文件——一致就接管（删掉临时副本），不一致就删掉重发；只有这条罕见路径才读目标文件算哈希。
- 发布顺序与压缩一致：文件数据 `sync_all` → `platform::fs::rename_durable` → 目录 flush → 入库。rename 走 `MoveFileExW(MOVEFILE_WRITE_THROUGH)`，不跨卷。
- 解码上限：单边 65535、总像素 1 亿、解码分配 512 MiB。超限进入失败清单。
- EXIF 方向由 `image` crate 的 `ImageDecoder::orientation()` 读取并 `apply_orientation`，只影响缩略图、pHash、主色与后续显示，不改原图字节。

### 导入时压缩（默认关闭）

- `ImportRequest.optimize` 为 `null` 时不压缩，行为与以前完全一致；勾选后只选输出格式（JPG / WebP）与质量档位（极高 / 高 / 中），复用批量压缩的 `QualityTier`、阶梯编码与 PSNR 裁决。勾选框旁写明「原图不会保留，无法还原」，开始导入前再确认一次。
- 位置在解码之后、发布之前；缩略图、pHash、主色都从同一张解码图计算，压缩前后像素相同，因此不需要重算。
- 压缩成功时：内容文件改用压缩结果，`content_hash` 取压缩后字节的 BLAKE3，扩展名与 `format` 跟随输出格式，`file_size` 用新长度，`optimized_at` 写当前时间；以后跑全库压缩会跳过这些行。
- 以下情形保留原图并照常导入（`optimized_at = 0`）：带透明通道但输出 JPEG、非 sRGB 色彩配置而输出装不下、单边超过 16383 且输出 WebP、16 位源、省不到 5%、编码失败。单张失败只写日志，不阻塞整批。
- 去重语义：内容哈希变成压缩后字节的哈希。同一源图用同样参数重复导入仍判重复（第二次会重新编码后在入库时命中同内容）；改档位或格式会变成两条记录。
- 不生成独立的 `optimize` 操作记录；导入撤销仍按 `origin_operation_id` 处理，压缩结果与原图走同一条导入记录。
- MCP 的 `import_start` 固定传 `optimize: null`，也不接受 `workerThreads`；`tasks_get` 的导入回执不返回 `optimized` / `optimizedBytes`，AI 不能触发或感知压缩。回执默认只给重复计数；`tasks_get(includePairs=true)` 才附带精确重复对、批内重复对、合并目标与源文件映射（各自上限 200，`similarPairs` 上限 100）。
- 完成页在勾选过压缩时显示「其中 N 张已压缩，省 X」；`N` 只统计真正入库的压缩版本，重复项不计。

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

- **精确重复**：`content_hash` 已存在。默认跳过并计入“重复跳过”；选择“合并元数据”时标签取并集、评分取较高值、更新 `modified_at`。合并前会记录评分、`modified_at` 与标签关系快照，撤销导入时恢复。回执同时给出 `duplicatePairs`（源文件 ↔ 已有图片 id、是否在回收站）与 `mergedImageIds`；图库不保存源文件名与源路径，这层对应关系只在导入过程中捕获。
- **批内重复**：工作线程用哈希占位，同一批内相同内容只保留一份；胜出源路径存在工作线程的 hash → 路径表里并跨 flush 保留。批内重复即使选择合并也按跳过处理，避免写入顺序不确定。回执给出 `batchDuplicatePairs`（`sourcePath` / `keptPath`）。
- **感知重复**：导入时把新 pHash 与库内 pHash 线性比较（默认汉明距离阈值 6/256），只记录提示，不自动跳过。任务开始时一次性载入库内 pHash 的 4×`u64` 紧凑数组；新入库图片在提交后追加。20 万规模约 6.4 MB 原始数据，S7 再加分桶/索引。MCP 回执里的 `similarPairs` 复用这些 `ImportHint`，每张新图只给最近的一张，阈值固定 6/256。
- 感知提示最多保留 100 条；结果里显示 `imageId / similarTo / distance`。
- 回收站中的同内容图片仍视为已存在；导入不会自动恢复。S3 已提供回收站界面，可在导入后手工恢复。
- 重复检测会把待释放的回收站行视为已存在（延迟释放的已知后果）；撤销导入只删除本次新增行，不删除用户源文件。

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

### 操作记录与导入撤销

- 导入开始前建一条 `pending` 操作记录（`HistoryKind::Import`）；记录 id 随 `ImportSource` / `WriterOptions` 进入写线程。冲突或线程启动失败时删掉空记录，已有的灰色撤销记录不受影响。
- 只有确实写入内容的批次才清空灰色（可重做）记录：导入被 write lane 冲突拒绝、或整批都是重复跳过时，重做分支必须保留。
- 每批事务里：新增行写 `origin_operation_id`；`affected` 累加新增与合并数量；新建文件夹/标签与合并快照按批追加 JSON chunk。不在内存里累积整次导入。
- 任务结束（完成、取消或失败）调用 `history::finish_live`；有任一已提交内容就转为可撤销，否则删除空记录。取消后已完成部分仍可撤销。
- 崩溃留下的 `pending` 导入记录在下次打开图库时按 `origin_operation_id` 或已有 chunk 判断转 `applied`，没有内容则删除。
- 撤销导入：按 `origin_operation_id` 分块把原图移入 `.artbank/undo/<op>/`，捕获完整图片行（含标签、集合位置、缩略图指针）后删行；恢复合并项的评分、`modified_at` 与标签关系；本次新建的文件夹与标签若已为空/无引用则删除，并捕获它们的完整行供重做。中断后重试是幂等的，记录保留为 `undone`。
- 重做导入：按原 id 插回行、把暂存原图移回文件夹、补标签与集合位置，恢复合并后的评分/时间；用撤销时捕获的行重建本次新建的文件夹与标签。导入暂存目录在重做后清空，下一次撤销会重新暂存。
- 缩略图包与新内容的 pHash/主色索引不需要回滚；缩略图 blob 留在追加式 pack 里，重做按原指针复用。新增行的 `origin_operation_id` 在重做后仍指向同一条记录，因此可以再次撤销。

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
target: "library" | "temp" 导入图库或临时文件夹；默认 library
folder: string             目标文件夹，图库内相对路径，根目录为 ""；target=temp 时忽略
folderMode: "flat" | "preserve"  目录层级；默认 flat
tags: string[]             初始标签
score: number              0-5
duplicatePolicy: "skip" | "merge"
perceptualThreshold: number 默认 6，后端限制在 0-64
optimize: null | { tier: "extreme" | "high" | "medium", outputFormat: "jpeg" | "webp" }
                            勾选导入时压缩才传对象；null 表示不压缩
```

`workerThreads` 不由前端传入：`start_import` 一律用设置里的 `performance.workerThreads` 覆盖请求值，为空时取 CPU 并行度 - 1（上限 128）。

`ImportProgress`（camelCase）：`jobId`、`phase`（`scanning` / `processing` / `finishing` / `completed` / `cancelled` / `failed`）、`target`、`total`、`processed`、`imported`、`duplicates`、`merged`、`tempReplaced`、`failed`、`similar`、`optimized`、`optimizedBytes`、`current`、`elapsedMs`。`ImportSnapshot` 另含 `startedAt`、`finishedAt`、`failures`、`hints`、`previewIds`，以及 `duplicatePairs` / `duplicatePairCount` / `duplicatePairsTruncated`、`batchDuplicatePairs` / `batchDuplicatePairCount` / `batchDuplicatePairsTruncated`、`mergedImageIds` / `mergedImageIdsTruncated`、`importedPairs` / `importedPairsTruncated`。四组数组各自上限 200，`hints` 上限 100；精确计数不受截断影响。

`tempReplaced` 只统计图库导入时被丢弃的临时区同内容行（见上面的“重复策略”），临时导入恒为 0。

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
- 临时导入：`target=temp` 平铺、跳过重复、不压缩、不写操作记录，拒绝 merge/preserve；关闭图库时行、原图与集合引用被清空；
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

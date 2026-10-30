# 导入与缩略图

导入把外部图片变成图库中的“原图 + 元数据 + 缩略图指针”。本文件覆盖导入管线（解码、BLAKE3、pHash、主色、重复策略、压缩、后台任务）、缩略图编码与追加式 pack、`thumb://` 协议，以及导入界面契约。

## 当前实现

### 输入与校验

- 文件选择器：多选图片；目录选择器：递归扫描。
- 拖放：Tauri 原生 `drag-drop` 事件；窗口任意位置可放下图片或目录。
- 支持格式：JPEG、PNG、BMP、WebP、TIFF、TGA、GIF（首帧）。HEIC/AVIF 暂不支持。缩略图始终取首帧，GIF 在网格与列表中不播放动画。
- 扫描跳过 `.artbank`、旧程序的 `.data` 与临时文件夹目录（`.artbank-temp*`）；若扫描范围在图库内，同时跳过 `recycle` 与 `output`；不跟随符号链接。
- 单次最多 10000 个输入路径、最多收集 100000 个文件；超出时停止扫描并在失败清单记录。
- 导入选项：目标（图库 / 临时文件夹）、目标文件夹（图库内相对路径）、目录层级模式（平铺 / 保留源目录）、初始标签（最多 32 个，单个 ≤64 字符）、初始评分 0-5、重复策略（跳过 / 合并元数据）。
- 初始标签使用与详情/批量编辑相同的 chip 编辑器：逐个输入并回车提交、自动补全、忽略大小写去重；聚焦且输入为空时显示常用标签，建议项带图片数；逗号、分号与空格不拆分，带空格的名称按一个标签处理。点击“开始导入”前会提交输入框中尚未回车的文本。
- 路径必须是绝对路径；目标文件夹经 `normalize_folder` 校验，拒绝 `..`、盘符与绝对路径。目标路径的任一级落在回收站或正在释放的文件夹时，导入在开始时拒绝并提示先恢复或永久删除。
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

### 导入时压缩（默认关闭，选项持久化）

- `ImportRequest.optimize` 为 `null` 时不压缩，行为与以前完全一致；勾选后只选输出格式（JPG / WebP）与质量档位（极高 / 高 / 中），复用批量压缩的 `QualityTier`、阶梯编码与 PSNR 裁决。勾选框旁写明「原图不会保留，无法还原」，开始导入前再确认一次。
- 选项持久化：打开对话框时读一次设置里的 `import.optimize`，回填开关、输出格式与档位；三个控件一改就写回去（关闭时存 `null`）。只记这三项，目标文件夹、目录层级、重复策略、评分与标签仍是每次现选。写失败只记 `console.warn`，不阻塞导入；临时导入（`target = temp`）既不读也不显示这一块。
- 存储与命令：`Settings` 顶层新增 `import: ImportSettings { optimize: Option<ImportOptimize> }`，默认 `None`；窄命令 `set_import_optimize(option)` 只改这一个字段后落盘，设置页 `save_settings` 提交整份 `Settings` 时后端保留该字段，两条写路径不会互相覆盖。不升 `SETTINGS_VERSION`：容器级 `serde(default)` 让旧文件缺这个键时取默认值。
- 位置在解码之后、发布之前；缩略图、pHash、主色都从同一张解码图计算，压缩前后像素相同，因此不需要重算。
- 压缩成功时：内容文件改用压缩结果，`content_hash` 取压缩后字节的 BLAKE3，扩展名与 `format` 跟随输出格式，`file_size` 用新长度，`optimized_at` 写当前时间；以后跑全库压缩会跳过这些行。
- 以下情形保留原图并照常导入（`optimized_at = 0`）：动画 / 多帧 / 多页源（GIF、APNG、动画 WebP、多页 TIFF，与批量压缩同用 `optimize::multiframe_guard`）、带透明通道但输出 JPEG、非 sRGB 色彩配置而输出装不下、单边超过 16383 且输出 WebP、16 位源、省不到 5%。
- 编码器异常与「判据不满足」分开计：编码报错按原图入库，同时累加 `optimizeFailed`，完成页显示「N 张压缩失败，已保留原图」；判据不满足不计数。两者都不阻塞整批。
- 去重语义：内容哈希变成压缩后字节的哈希。同一源图用同样参数重复导入仍判重复（第二次会重新编码后在入库时命中同内容）；改档位或格式会变成两条记录。
- 不生成独立的 `optimize` 操作记录；导入撤销仍按 `origin_operation_id` 处理，压缩结果与原图走同一条导入记录。
- MCP 的 `import_start` 固定传 `optimize: null`，也不接受 `workerThreads`；`tasks_get` 的导入回执不返回 `optimized` / `optimizedBytes`，AI 不能触发或感知压缩。回执默认只给重复计数；`tasks_get(includePairs=true)` 才附带精确重复对、批内重复对、合并目标与源文件映射（各自上限 200，`similarPairs` 上限 100）。
- 完成页在勾选过压缩时显示「其中 N 张已压缩，最终新增 X，省 Y」；`N` 只统计真正入库的压缩版本，重复项不计；`X` 是本次新增图片的最终字节总和，压缩过的按压缩结果算。有编码失败时另起一行「N 张压缩失败，已保留原图」。

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
- **感知重复**：导入时把新 pHash 与库内 pHash 线性比较（默认汉明距离阈值 6/256），只记录提示，不自动跳过。任务开始时一次性载入库内 pHash 的 4×`u64` 紧凑数组；新入库图片在提交后追加。20 万规模约 6.4 MB 原始数据；分桶/索引尚未做。MCP 回执里的 `similarPairs` 复用这些 `ImportHint`，每张新图只给最近的一张，阈值固定 6/256。
- 感知提示最多保留 100 条；结果里显示 `imageId / similarTo / distance`。
- 回收站中的同内容图片仍视为已存在；导入不会自动恢复，可在导入后从回收站手工恢复。
- 重复检测会把待释放的回收站行视为已存在（延迟释放的已知后果）；撤销导入只删除本次新增行，不删除用户源文件。

### 缩略图包与指针

- 缩略图最长边 512px、WebP quality 80；透明图走 RGBA。
- 追加式 `.pack`：`0001.pack` 起、16 字节段头（`ARTBTHM1` + u32 版本 + 4 字节保留）、默认 1 GiB/段。
- 写入由单写通道串行执行，每个 blob 后 flush；读取按 segment 缓存句柄并定位读，单 blob 上限 32 MiB。
- 不做逐 blob fsync：累积 8 MiB、换段、指针事务提交前（每批入库、缩略图重建、编辑保存）与关闭图库时 `sync_all`。掉电最多丢掉最后一段未落盘的字节，打开图库时按 `thumb_segments.size` 比对磁盘长度，把越界指针清成 `thumb_segment = 0`，这些图重新算「缺缩略图」并由重建任务补上。
- 每批事务内写入 `thumb_segment / thumb_offset / thumb_length / thumb_format / thumb_width / thumb_height / thumb_version = 1`，同时维护 `thumb_segments` 行与段文件尺寸。
- 进程崩溃最多留下未引用的 blob；已提交行不受影响。pack 丢失或指针失效时协议返回 404，由前端显示占位，不会崩溃。

### `thumb://` 协议

- URL：`thumb://<imageId>?v=<thumbVersion>`。Windows 的 WebView2 只接受 `http://thumb.localhost/<imageId>?v=<thumbVersion>`；前端统一经 `src/shared/thumbUrl.ts` 的 `thumbnailUrl()` 生成，不自己拼字符串。
- 查询串只作为浏览器缓存键，服务端只解析路径里的 id，不校验 `v`。
- 响应头：`Content-Type` 取自 `thumb_format`（当前只有 `image/webp`）、`Content-Length`、`Cache-Control: public, max-age=31536000, immutable`、`ETag: "v<thumb_version>"`、`X-Content-Type-Options: nosniff`。
- 状态码：id 不是整数 → 400；图库未打开、图片无缩略图、pack 缺失或指针越界 → 404；数据库与文件读取异常 → 500。错误响应带 `Cache-Control: no-store`，不会被长缓存钉住。网格/列表在图库打开后按 `thumb_version` 请求；404 时显示占位，点击重试会附加 `retry` 查询参数。

### 并发、进度与取消

- 像素工作走全局图像预算：线程数取设置里的 `workerThreads`，为空时使用逻辑核数 - 1；每处理一张前拿一个许可，导入、压缩、估算、缩略图、查看器、AI 解码与 MCP 拼图共用同一个上限。
- 导入在解码前申请字节预算：只读文件头取尺寸，不压缩按“像素数 × 5 字节 + 8 MiB”，压缩按“像素数 × 16 字节 + 源文件大小 + 8 MiB”。额度全进程一份、上限 1.5 GiB，导入、空间优化、估算样本编码、查看器预览、AI 输入解码与 MCP 拼图共用；申请等待可取消；单张估算超过上限时独占——等当前占用归零后放行，避免超大图永远排不到，也不与其它解码叠加。额度在特征计算与压缩结束、发布文件之前释放，不等写线程。
- 压缩路径直接消费已解码的 `DynamicImage`（RGB8/RGBA8 不再复制一份 `PixelBuffer`）；不搬运 EXIF 的源文件字节在 ICC/色域判断后先释放。导入收尾的 debug 级日志记录线程数、预算上限、实际占用峰值与单张最大估算。
- 结果是深度 128 的有界通道送入唯一写线程；每 64 张或 250ms 批量提交；重复项最多攒 128 条。工作线程用原子索引动态取下一个文件，每个文件恰好处理一次。
- 每张图的阶段：读取/哈希 → 解码/特征 → 发布 → 入库。进度按文件数加权：处理阶段 0–90；工作线程结束后进入收尾，按写线程已处理（成功、重复或失败）的条数推进 90–99，`phase=completed` 才 100%。
- 进度另给出“最终新增体积”与预计剩余：写线程每批提交后，把本批新增行（不含重复与合并）的 `file_size` 求和累加；压缩成功行的 `file_size` 已是压缩结果，因此口径就是最终增加的字节。`ratePerSecond` 从第一次进入运行阶段起按“已写入张数 / 已运行秒数”折算，不足 0.5 秒或尚未写入时为空；`etaSeconds = (total − written) / rate`，终态为空。计时起点与任务总线一致，不做滑动窗口。
- `import-progress` 事件每 150ms 推送一次；`import-finished` 在结束时推送完整快照。导入同时挂在任务总线上，右下角任务中心也能看到它与取消入口。
- 默认排队：`start_import` 立即返回快照，`phase` 为 `queued`（等待资源）或 `scanning`；图库导入申请会话读、时间线读、内容写者与目标文件夹子树读，临时导入不占时间线。其他导入、压缩、AI 批量任务与目标子树内的文件夹写会等待，其他子树的文件夹操作可以并行。可以同时排队多个导入，但运行中的导入仍只有一个。
- 取消：工作线程在文件边界停止取新文件；已准备好的批次继续由写线程落库，临时文件清理后结束，终态为已取消。
- 运行中的导入只允许一个，后续导入排队；关闭、创建或切换图库要等导入与写任务结束。
- 写线程按文件夹路径缓存 `folder_id`，同一批次重复进入同一目录时只解析/插入一次。
- 工作线程 panic 时 `pool.install` 会直接展开，正常路径的 `join` 就执行不到；写线程由 `WriterGuard` 的 Drop 兜底 join，任何退出路径都先排空通道，再让 `finish` / `LiveOperation` 收尾。记录行已经不在了（收尾把空记录删掉）时 `append_live_in_tx` 跳过逆数据写入并记警告，不让一个外键失败带走整批已入库的图片。
- 失败清单最多保留 200 条；每条包含路径与原因。
- 结果快照保留最近 24 个新增图片 id，用于导入完成后的缩略图预览。
- 内存峰值由字节预算控制，而不是“并发度 × 单图解码”：典型尺寸的图仍能满线程并行，大图会自动降到少量并发。工作线程全部处理完后显示“正在写入最后一批（N 张）”。
- 导入任务挂到任务总线时，快照额外带 `addedImages`（累计新增行数）、`removedTempImages`（图库导入丢弃的临时区同内容行）与 `importTarget`；前端据此增量更新图库/临时区计数，不再为一次导入做全库计数。

### 操作记录与导入撤销

- 导入开始前建一条 `pending` 操作记录（`HistoryKind::Import`）；记录 id 随 `ImportSource` / `WriterOptions` 进入写线程。记录由 `history::LiveOperation` 守卫：线程启动失败、panic 或提前返回时在 Drop 里删除空记录；已有内容则保留为可撤销。已有的灰色撤销记录不受影响。
- 只有确实写入内容的批次才清空灰色（可重做）记录：导入因资源冲突被拒绝、或整批都是重复跳过时，重做分支必须保留。
- 每批事务里：新增行写 `origin_operation_id`；`affected` 累加新增与合并数量；新建文件夹/标签与合并快照按批追加 JSON chunk。不在内存里累积整次导入。
- 任务结束（完成、取消、失败或 panic）由 `LiveOperation` 收尾；有任一已提交内容就转为可撤销，否则删除空记录。取消后已完成部分仍可撤销。
- 崩溃留下的 `pending` 导入记录在下次打开图库时按 `origin_operation_id` 或已有 chunk 判断转 `applied`，没有内容则删除。
- 撤销导入：按 `origin_operation_id` 分块把原图移入 `.artbank/undo/<op>/`，捕获完整图片行（含标签、集合位置、缩略图指针）后删行；恢复合并项的评分、`modified_at` 与标签关系；本次新建的文件夹与标签若已为空/无引用则删除，并捕获它们的完整行供重做。中断后重试是幂等的，记录保留为 `undone`。同一次导入里同一张图被合并多次时只应用最早那份快照，恢复成导入前的状态。
- 重做导入：按原 id 插回行、把暂存原图移回文件夹、补标签与集合位置，恢复合并后的评分/时间；用撤销时捕获的行重建本次新建的文件夹与标签。导入暂存目录在重做后清空，下一次撤销会重新暂存。撤销时原图就已经缺失（或暂存文件已被清理）的图片照常插回行、文件留空并记入失败清单，不阻塞同一条记录里的其它图片；这些行由检查图库按「缺失」报出。
- 缩略图包与新内容的 pHash/主色索引不需要回滚；缩略图 blob 留在追加式 pack 里，重做按原指针复用。新增行的 `origin_operation_id` 在重做后仍指向同一条记录，因此可以再次撤销。

## 前端契约

- 命令入口：`src/modules/import/api.ts`。
- 类型：`src/modules/import/types.ts`。
- 界面：`src/modules/import/ImportDialog.tsx`，由 `src/app/LibraryWorkspace.tsx` 打开；拖放监听在工作区，弹窗打开时由弹窗接管新增路径。运行中弹窗模态阻塞，右上角关闭、Esc 与点击遮罩都不可用，只能“取消导入”；标题显示阶段名，正文显示当前文件与百分比，进度行同时显示已写入体积、运行中速度与预计还需。
- 字段里含多个可聚焦控件（初始标签的 chip 删除按钮、目标文件夹输入框与选择按钮、输出格式按钮组）时使用 `Field` 的 `plain` 模式渲染成 `div` 而不是 `<label>`：`<label>` 会把字段内任意点击转发给第一个可聚焦后代，曾导致点提示文字误删标签。单输入框字段仍用 `<label>` 保留点击聚焦。目标文件夹使用与侧栏相同的 `FolderPickerDialog`，打开时预填当前浏览的单选文件夹（多选或根目录时为空）；目录层级用“平铺 / 保留源目录”两个按钮切换。初始标签使用共享 `TagInput`，标签建议由 `LibraryWorkspace` 注入的 `searchTags` 提供（返回名称与图片数，聚焦空输入显示常用标签），导入模块不直接依赖 `libraryApi`。
- 导入完成时工作区递增浏览刷新令牌，`LibraryView` 重新查询第一页并刷新图库计数；筛选条件保留，图片选择清空。结果弹窗仍显示最近 24 张预览，关闭后回到网格。

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `start_import` | `{ request }` | `ImportSnapshot`（立即返回；`phase` 可能为 `queued`，任务在后台执行） |
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

`workerThreads` 不由前端传入：导入的工作线程数取全局图像预算（设置里的 `performance.workerThreads`，为空时取逻辑核数 - 1，上限 128）；请求里的 `workerThreads` 字段仅为反序列化兼容保留，不再生效。

`ImportProgress`（camelCase）：`jobId`、`phase`（`queued` / `scanning` / `processing` / `finishing` / `completed` / `cancelled` / `failed`）、`target`、`total`、`processed`、`written`、`percent`、`imported`、`duplicates`、`merged`、`tempReplaced`、`failed`、`similar`、`optimized`、`optimizedBytes`、`writtenBytes`、`ratePerSecond`、`etaSeconds`、`optimizeFailed`、`current`、`elapsedMs`。`writtenBytes` 是已入库新增图片的最终字节总和；`ratePerSecond` 为 `null` 表示样本不足或尚未开始，`etaSeconds` 在终态或没有速度时为空。`written` 是写线程处理完（成功、重复或失败）的条数；`percent` 在 completed 之前最多 99。`ImportSnapshot` 另含 `startedAt`、`finishedAt`、`failures`、`hints`、`previewIds`，以及 `duplicatePairs` / `duplicatePairCount` / `duplicatePairsTruncated`、`batchDuplicatePairs` / `batchDuplicatePairCount` / `batchDuplicatePairsTruncated`、`mergedImageIds` / `mergedImageIdsTruncated`、`importedPairs` / `importedPairsTruncated`。四组数组各自上限 200，`hints` 上限 100；精确计数不受截断影响。

`tempReplaced` 只统计图库导入时被丢弃的临时区同内容行（见上面的“重复策略”），临时导入恒为 0。

事件名：`import-progress`、`import-finished`。

## 尚未实现

- 剪贴板图片/路径输入。
- HEIC/AVIF 解码。
- 移动模式；默认只复制，不改用户原文件。源目录若在图库内，原文件也不会被移动或删除；请优先从图库外导入。
- 感知重复的分桶索引与阈值标定（需真实样本，见[待办](../planning/todo.md)）。

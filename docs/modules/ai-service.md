# AI 服务接口

description、image/text embedding、pose 都通过本机 HTTP 服务完成。图库本体不依赖模型进程；服务离线时相关按钮灰显，浏览、导入、编辑不受影响。

服务端代码在仓库 `server/`，实现下面的 `/v1` 契约；Python 环境与模型文件不随包分发，由用户放进程序目录。应用仍只依赖 `/v1`，因此也可以连接其他实现。

## 进程托管

设置项（当前设置 v7 的 `ai` 段）：

```text
ai.baseUrl         默认 http://127.0.0.1:18766；探活与所有请求都走这里
ai.port            默认 18766；启动 Python 时传 `--port`，与本机 baseUrl 的端口需一致
ai.autoStart       随应用启动
ai.pythonPath      默认 python_embeded\python.exe；为空时只连接外部已启动的服务
ai.serverScript    默认 server\main.py
ai.modelPath       模型根目录，默认 resources\models
ai.clipModelPath   CLIP 模型目录，默认 siglip2-giant-opt-patch16-384；相对 modelPath，也可填绝对路径
ai.prewarmImports  默认 true；启动托管服务时加 --prewarm-imports，在后台导入 torch/transformers
```

- 相对路径以程序根目录解析：release 为 exe 所在目录，debug 为仓库根目录；两种模式都可用 `LILITH_ARTBANK_ROOT` 覆盖。
- 设置文件固定为程序根目录的 `settings.json`；日志在 `logs/`，Python 的 stdout/stderr 写入 `logs/ai-server.log`。
- 启动命令为 `<pythonPath> <serverScript> --port <port> --model-dir <解析后的 CLIP 目录>`，工作目录为程序根目录，不弹控制台。
- 保存启动相关设置后，托管的服务会按新配置重启；外部服务不受影响。
- 程序目录不可写时直接报错，不回退到 AppData，也不从旧 AppData 设置迁移。

生命周期：

1. 启动后先 `GET /v1/health` 探活。成功则标记在线，不重复拉起。
2. `autoStart`、本机 `baseUrl`、Python 与脚本都配置好且探活失败时启动进程；等待健康检查最多 60 秒。
3. 端口被其他程序占用且健康检查不是本服务格式 → 标记不可用，提示端口冲突；不尝试杀进程。
4. 启动失败按 2s、4s 退避重试，最多 3 次；仍失败则记录状态与错误并保持离线，由用户在设置页或 AI 任务里手动重启。运行中被托管的进程退出时标记为失败，不自动重启；正在执行的任务按单张失败处理，不会因为服务退出而无限重试。
5. 退出时优先调用 `POST /v1/shutdown`（返回 2xx 即视为已关闭），超时后终止子进程。
6. 服务只绑定 `127.0.0.1`。v1 不做鉴权；`baseUrl` 不是本机地址时只连接外部服务。
7. 监督线程用「代次 + 持有者」交接：启动中保存设置或停止服务会让旧线程作废，新线程在同一把锁里接手，旧线程退出时清不掉新一代的标记，所以不会出现「此后自动拉起静默失效」。

模型由首次使用自动加载：批量任务、单张 pose/描述与语义搜索在需要时调用 `/v1/models/ensure`；设置页与批量任务对话框只显示“已加载 / 未加载”，不提供手动加载按钮。

Python 环境与模型文件不打包、不下载；`server/` 代码随便携包分发。

## 通用约定

- 前缀 `/v1`。
- 图片输入：`Content-Type: application/octet-stream`，body 为图片文件原始字节（JPEG/PNG/WebP/BMP 至少支持）。不要 base64；避免额外 33% 体积。
- 文本输入：`Content-Type: application/json; charset=utf-8`。
- 成功响应：JSON。错误响应：`{"error":"...", "message":"..."}` + 合适 HTTP 状态码。
- 状态码：
  - 400 请求格式错误；
  - 404 能力/模型不存在；
  - 413 请求体超限；
  - 422 输入无法解码或模型未就绪；
  - 500 推理失败；
  - 503 服务忙/模型加载中。
- 超时：health 5s，embedding / pose / description 单次尝试 120s，`/v1/models/ensure` 600s；连接超时 5s。
- 预算：一次逻辑调用最多 3 次尝试、总预算 5 分钟（`ensure` 为 2 次、11 分钟），重试与退避都算在里面。
- 取消：任务取消时正在飞的请求立即返回，退避等待也会被打断；被打断的那次不算失败，不进失败清单。
- 重试：网络错误与 5xx 最多 2 次（250ms、1s 退避）；4xx、响应格式错误与超预算不重试。
- 并发：客户端固定串行（1 个在途推理）；服务端同时最多处理 4 个推理请求，超出的直接回 503 `server_busy`，不排队。
- 请求体上限：建议 64 MiB；服务端单张解码上限 1 亿像素（与导入一致），超过回 422。
- description 输入应使用降采样预览而不是 100MP 原图。

## 能力

| 能力 | 输入 | 输出 | 维度/单位 |
| --- | --- | --- | --- |
| `imageEmbedding` | 图片字节 | `vector: number[]` | `siglip2-giant-opt-patch16-384`，1536，L2 归一化 |
| `textEmbedding` | `{text}` | `vector: number[]` | 同 image embedding 模型，1536，L2 归一化 |
| `pose` | 图片字节 | 33 个 landmark | MediaPipe Pose 归一化坐标 |
| `description` | 图片字节 + 可选 prompt | `{text}` | 文本；模型以服务端为准 |

image 与 text embedding 必须来自同一模型/维度。服务在响应里返回 `model` 与 `dim`；应用以响应和 health 为准，只比较相同 `model` 与 `dim` 的向量。模型变更后旧向量保留但不参与检索，由维护任务重算或清理。客户端落盘时把 embedding 再归一化一次并存 fp16；pose 存 f32。

## 端点

### GET /v1/health

```json
{
  "status": "ok",
  "version": "1",
  "capabilities": {
    "imageEmbedding": { "ready": true, "model": "siglip2-giant-opt-patch16-384", "dim": 1536 },
    "textEmbedding": { "ready": false, "model": "siglip2-giant-opt-patch16-384", "dim": 1536 },
    "pose": { "ready": true, "model": "mediapipe-pose" }
  }
}
```

- 未加载的模型 `ready=false`；应用可调用 `/v1/models/ensure` 再试。
- CLIP 能力在未加载时仍必须返回 `model` 与 `dim`，语义搜索据此校验向量，不必为了拿维度去加载图片塔。
- 图片塔与文本塔同一时间最多一个 `ready=true`；空闲卸载后两个都为 false。
- 没有该能力时整个字段可省略。服务端不提供 `description`，`/v1/describe` 返回 404；没有可读的 CLIP 模型目录时，两个 CLIP 能力也整体缺省，此时客户端报“没有图像 embedding 能力”。

### POST /v1/models/ensure

按需加载模型，幂等；可能耗时数十秒到数分钟。

```json
{ "capabilities": ["imageEmbedding", "pose"] }
```

响应 `{ "capabilities": { ... } }`，结构同 health。加载失败返回 422/503；失败的塔不会保留半加载状态。

`ensure` 在把权重映射进内存之后会**先跑一次前向**（warmup）再返回：权重是按需分页的 mmap，只 mmap 时 RSS 约 499 MB，首次 forward 才把 2.83 GB 真正读进物理内存（耗时见[性能基准](../guides/performance.md)）。所以“加载完成”等于“可以直接用”，这段时间算在 ensure 的耗时里而不是第一次搜索里。warmup 失败按加载失败处理。

- 一次请求最多接受一个 CLIP 塔；同时请求 `imageEmbedding` 与 `textEmbedding` 返回 400。应用分别加载图片塔与文本塔。
- 请求 `description` 返回 404；当前服务端没有描述模型。
- 加载图片塔会让已驻留的文本塔让位，反之亦然。

### POST /v1/embed/image

```http
POST /v1/embed/image?model=siglip2-giant-opt-patch16-384
Content-Type: application/octet-stream

<raw image bytes>
```

```json
{
  "status": "success",
  "model": "siglip2-giant-opt-patch16-384",
  "dim": 1536,
  "normalized": true,
  "vector": [0.0123, -0.0456, ...]
}
```

图片塔未加载时返回 503；批量 embedding 任务在开始前调用 `/v1/models/ensure` 加载图片塔。

### POST /v1/embed/text

```json
{ "text": "穿红色斗篷的侧身人物", "model": "siglip2-giant-opt-patch16-384" }
```

响应同 image embedding。服务端负责分词与长度截断（当前按 64 token 截断），超长文本不报错，在 `truncated` 字段说明。文本塔未加载时返回 503；应用在单次语义搜索前调用 ensure。

### POST /v1/pose

```http
POST /v1/pose?model=mediapipe-pose
Content-Type: application/octet-stream

<raw image bytes>
```

```json
{
  "status": "success",
  "model": "mediapipe-pose",
  "imageWidth": 1920,
  "imageHeight": 1080,
  "landmarks": [
    { "x": 0.51, "y": 0.22, "z": -0.03, "visibility": 0.99 },
    ...
  ]
}
```

- 33 个点，顺序沿用 MediaPipe Pose；`x/y` 归一化到 [0,1]，`z` 为相对深度，`visibility` 0-1。
- 未检测到人体时返回 `landmarks: []` 或 `status: "success"` + 空数组；不要返回 500。

### POST /v1/describe

```http
POST /v1/describe?prompt=用中文简短描述画面、风格与构图&maxTokens=160
Content-Type: application/octet-stream

<raw image bytes>
```

```json
{
  "status": "success",
  "model": "qwen-vl",
  "text": "一名穿深色斗篷的人物侧身站在雨夜街道，冷色调，构图偏左。",
  "truncated": false
}
```

- 同步返回；长任务由客户端队列控制并发。
- 服务端可对图片做内部缩放，但输入已由客户端尽量控制在 1536px 以内。
- 服务端不实现描述，`/v1/describe` 返回 404；只有批量任务里的描述项置灰，详情里的单张“AI 生成”按钮仍可点，服务返回 404 时报错。

## 应用侧任务

### 任务类型

- `embedding`：为选中/当前视图/全库缺失的图片生成 image embedding。
- `pose`：为选中/当前视图/全库缺失的图片生成 pose。
- `description`：为选中/当前视图/描述为空的图片生成描述；默认只填空，可选覆盖。服务端没有描述模型，界面置灰。
- `textSearch`：只做一次 text embedding，然后本地扫描。

### 队列与进度

- 批量任务挂在 `app/tasks/` 的任务总线上，进度、取消与失败清单都由 `TaskHandle` 承载；进度事件 `task-progress` / `task-finished` 约 150ms 一次，失败清单最多保留 200 条（imageId + 原因）。`ai-job-progress` / `ai-job-finished` 已删除，前端只订任务总线。
- 范围由 `TaskScope` 在 Rust 侧解析成 id 集合（选中 / 文件夹 / 集合 / 当前视图 / 临时文件夹 / 全库）；`AiJobRequest` 只接收已解析的 id，不再有“id 与 query 都为空就退回全库”的隐式分支。
- 开始前预览：`preview_batch_task` 只读返回“范围 N 张 / 将处理 M 张”。待处理条件与任务启动共用 `query::PendingFilter`：embedding 按当前模型与维度、pose 按任意模型、描述按空描述或非手写来源；已删除与待释放行不计入 M。AI 离线或 embedding 缺模型维度时 M 显示“—”，任务不可开始；服务恢复后由状态轮询触发重算。
- 快照字段：除张数与百分比外，`TaskSnapshot` 还带 `runStartedAt`（第一次进入 `running` 的时间，排队与准备不算）、`currentFile`（图库内相对路径）、`ratePerSecond` 与 `etaSeconds`；压缩任务另有 `originalBytes` / `savedBytes`（只统计已提交并替换的图片）与 `skipBreakdown`（候选内跳过按 6 个原因细分）。速度是运行以来平均每秒结算的张数，含失败与跳过；压缩的 ETA 优先用滚动吞吐，百分比由「已结算 / 候选张数」派生（`progressPercent` 对压缩恒为 `null`）；终态不返回 ETA。
- 冲突：AI 任务占 `ai.service` 与内容写者，同一时间只运行一个 AI 批量任务，不会与导入、移动、编辑、压缩等内容写任务同时运行；描述任务还占时间线读与元数据写。每张读原图时再按文件夹加短读锁并拿图像预算许可，因此文件夹维护不必等整个 AI 任务结束。默认自动排队：`start_batch_task` 直接返回 `queued` 快照，任务在 `queued` 阶段等待，不需要前端选择排队或取消现有任务。排队中的任务在任务中心可取消；取消排队任务只把它从等待队列移除，不触碰正在运行的任务。
- 先到先得：同一资源上的等待者按申请顺序启动；路径锁写者优先，避免文件夹改名被持续的读图请求饿死。短写只申请短期资源，不排在长任务后面。
- 取消：停止调度新请求；正在飞的那次请求约 100 毫秒内被打断，不算失败、不进失败清单；已经保存的结果不回滚。
- 熔断：连续 5 张失败视为服务不可用，任务以失败结束，避免把整库都记成失败。
- 发送的图片：embedding 与 pose 使用缩略图包里的 512px WebP（缺失或读取失败时回退解码原图）；description 解码原图（应用 EXIF 方向）后缩到最长边 1536，避免把 100MP 原图直接上传。回退解码原图时按像素申请全进程 1.5 GiB 字节预算，与导入、压缩、查看器共用。
- 已完成的 embedding/pose 写入 feature pack 与 `feature_blobs`；描述写入 `images.description`，并设置 `description_source='ai'`、`description_model`。用户手工编辑后 `description_source='user'`。
- 描述任务会产生用户可见的内容修改，因此使用 live 操作记录：任务开始建 `pending` 记录，每张写入时在同一事务里追加旧描述并计入 `affected`，结束（含取消/失败）调用 `finish_live`；已经写入的描述在任务取消后仍可撤销。该记录由 `history::LiveOperation` 守卫，任务 panic、线程启动失败或提前返回时在 Drop 里收尾，不会留下永远 pending 的记录。单张 `ai_describe_image` 走普通 `history::write`。embedding/pose 是派生数据，不进入操作记录。
- 任务 panic 按失败处理：任务边界用 `catch_unwind` 捕获，标记失败、写失败清单并释放资源；不会再出现“界面显示运行中、资源永不释放”的状态。release 构建使用 unwind，panic 信息写入日志。
- 任务状态不持久化；崩溃后已保存结果保留，重跑时默认只处理缺失项。

## 存储

```text
<library>/.artbank/features/
  embedding-<model-slug>-1536-f16-<hash8>.pack   raw little-endian f16 records
  pose-<model-slug>-132-f32-<hash8>.pack         raw little-endian f32 records
```

- 文件名由 kind、模型 slug（小写、非字母数字折叠为 `-`、最多 48 字符）、维度、dtype 与模型名 BLAKE3 前 8 位组成，同一模型名始终落到同一个包。
- 包头 32 字节：magic `ARTBFTR1`、version、kind、dtype、dim、record_len、创建时间（14-15 字节保留）；`record_len` 必须等于 `dim × 2`（f16）或 `dim × 4`（f32）。
- 追加式写入，`feature_blobs` 记录 `(image_id, kind, model, dim, pack_name, pack_offset, pack_length)`；同一图片重算时追加新记录并更新指针。旧字节不回收，只在维护页的「特征包」里统计（实测 0 垃圾；模型切换会让整包作废重算，压了没有意义）。
- 追加只 `flush`，累积 4 MiB 或关闭/切换图库时 `sync_all`；重新打开写句柄时若文件末尾不足一条记录（掉电留下半条），先截到整条边界再继续追加。读取按偏移校验长度，越界即报错，不会返回半条向量。
- `feature_blobs.dim` 对 embedding 是向量维度，对 pose 是 landmark 个数（33）；pose 包的 `dim` 是 f32 元素数 132（33 × 4），两者不要混用。
- embedding 磁盘上每条 `dim × 2` 字节（仅存 fp16）；读入后立即转 fp32，均值、点积、范数与余弦全部用 f32 在 CPU 计算。pose 每条 33 × 4 × 4 字节（f32，x/y/z/visibility）。
- 读取向量不整体载入：扫描按 `pack_offset` 排序后 mmap 顺序读，数据库只做一次批量查询；单条读取（查看器 pose）按偏移定位。
- 行指针查询只读 `feature_blobs`，不再 `JOIN images` 判断可见性：回收站（以及全库范围下的临时区）的 id 先用两条小查询取成集合，再在内存里减掉。真实库上这省掉数十万次回表，而这两个集合通常是空的。

## 服务端实现（server/）

- 入口 `server/main.py`：标准库 `ThreadingHTTPServer`，只绑 `127.0.0.1`；路由 `/v1/health`、`/v1/models/ensure`、`/v1/embed/image`、`/v1/embed/text`、`/v1/pose`、`/v1/shutdown`；`/v1/describe` 返回 404。
- 推理并发上限 4（`MAX_INFERENCE_CONCURRENCY`）：名额满时立刻回 503 `server_busy`，不排队，避免并发请求一起放大显存与内存；`/v1/health` 与 `/v1/shutdown` 不占名额，并发推理时也能探活与关闭。
- `clip_cache.py`：从同一份 safetensors 索引只读取 `vision_model.*` 或 `text_model.*`，不实例化另一塔；CUDA 用 fp16，CPU 用 fp32；同一时间只驻留一个塔，加载另一塔时旧塔立即让位；空闲 300 秒自动卸载（`--idle-unload-seconds`，0 表示关闭）。health 不等待加载或推理。加载完成后立即 warmup 一次前向。
- `--prewarm-imports`：启动后在后台线程导入 torch/transformers（实测 7.7–8.0 s、RSS 420 MB），把这段成本从“第一次 AI 请求”挪到“用户还在看图”。没有模型目录时不预热。应用按设置 `ai.prewarmImports`（默认开）决定是否传这个参数。
- `pose_engine.py`：MediaPipe Pose（`model_complexity=2`、`static_image_mode=True`、`min_detection_confidence=0.3`）。模型随 `python_embeded` 携带，不读 `resources/models`；未检测到人体返回空数组。
- `image_io.py`：PIL 解码、EXIF 方向、RGB 转换；单张上限 1 亿像素（`MAX_PIXELS`，与导入解码上限一致），超过直接拒绝；解码失败或超限返回 422。
- 运行环境为 Python 3.11 + torch 2.5.1+cu121 + transformers 4.57.6 + mediapipe 0.10.14，依赖已在 `python_embeded` 内，不做运行时下载。
- 日志写 `logs/ai-server.log`；只记录启动参数、模型加载/卸载、耗时与错误，不记录图片内容或向量。
- 描述能力未实现；`/v1/describe` 返回 404，只有批量任务里的描述项置灰。

## 本地检索

相似度有三类数据源：常驻视觉索引（pHash + 4/8 主色）、feature pack（embedding）、feature pack（pose）。三者都只返回 id + 分数，按 token 分页后再回填元数据。

- **常驻视觉索引**（`library/similarity.rs` 的 `VisualIndex`）：载入 `images` 中 `phash`/`colors4`/`colors8` 完整、未删除的行（临时区行带 `is_temp` 标记），pHash 存 4×u64，两套主色预转 OKLab（`colors::prepare_palette`），20 万行约 46 MB。用 `count(*) + max(id)` 判断是否仍然有效，导入等新增行会让它重建；编辑、移入/恢复/永久删除、清空回收站经 `invalidate_similarity()` 显式失效。全库扫描与相似分组（任何范围）都跳过临时行；显式范围仍可用于找相似与语义搜索。
- **pHash 相似**：参考图 256 位哈希，Hamming 距离升序、同距离按 id 升序；遍历常驻索引做位运算扫描，不再查库。只支持单张参考图。
- **主色相似**：参考主色（单张的 4/8 色，或多张等权合并后的代表色）与索引逐项做对称加权 OKLab 距离（每项匹配对方最近色、按自身权重平均，两个方向再取均值），距离升序。
- **均值中心化**：打开图库后在后台线程扫描当前 model/dim 的普通图 embedding，求 fp32 均值并保存在内存；不阻塞开库，也不随新增/编辑更新，关闭并重新打开图库刷新。MCP 的临时区特征生成完成后会显式失效均值缓存。均值按 4096 行分块用 rayon 并行累加再合并（主库全量 × 1536 维）；语义搜索遇到 `Computing` 时等待后台结果（最多 15 s）而不是同步重算。
- **语义搜索**：text → text embedding → `cosine((v - mean), (q - mean))` 与相同 model/dim 的 image embeddings 扫描。慢阶段通过 `semantic-stage` 事件上报（加载文本模型 / 生成查询向量 / 计算均值 / 扫描向量），界面显示在搜索框下方。比较时同时计算 `dot(v, q)` 与 `dot(v, mean)`，由 `||v||≈1` 还原中心化后的范数，保持精确余弦。客户端只加载文本塔；图片模型名与维度从 health 读取，不触发图片塔加载。
- **相似 embedding**：参考图的 image embedding（单张，或多张平均后归一化）同样减均值后扫描。参考图没有 embedding 时直接报错要求先跑 AI 任务，不在查询路径里顺手生成。
- **平均参考**：多张参考图只支持主色与 embedding。主色把每张图的 4/8 色按等权合并后，在 OKLab 做加权 k-means（初始质心按权重最高、随后每次取最远点，不用随机种子），保留同样数量的代表色；embedding 先求平均再归一化。所有参考图都会从结果中排除；缺任一特征、模型/维度不一致或超过 100 张时整次拒绝。
- **范围**：找相似与语义搜索的后端都接受 `TaskScope`（当前选择/文件夹/集合/视图/临时文件夹/全库）；界面只在找相似面板里提供范围选择，语义搜索目前只有 MCP 侧会传 `scope`。全库不解析 id；其它范围由 `TaskScope::resolve` 先用现有查询取 id 集合，再与扫描取交集。embedding/pose 在读取向量前就丢掉范围外的行，主色/pHash 在常驻索引扫描时跳过范围外的行，不退回 O(n²)。**全库（`scope` 省略或 `library`）不包含临时文件夹里的图片**；`temp`、显式 id 或 `view=temp` 等显式范围可以命中临时图。embedding 均值默认也排除临时区；库里只有临时特征时回退到包含临时区，保证 `scope=temp` 仍能搜索。
- **阈值**：pHash 是 0–256 距离上限；主色是 0–1 的 OKLab 距离上限；embedding、pose、语义是 -1–1 的相似度下限。`total` 只统计范围与阈值过滤后的命中。
- **姿势相似**：`ai/pose_similarity.rs`，33 点按躯干、四肢 2D/3D、PCA、径向签名、凸包面积加权，镜像后取较大值；每个 pose 包 mmap 一次后顺序切片。移植自旧程序 `MediaPipePoseData::similarity`，现在按本仓库自己的算法维护，权重与索引可以为精度或性能调整（`pose_similarity.rs` 的 golden 值测试要随改动一起更新）。移植来历见 `docs/planning/archive/2026-10-23-S5取消与AI模型调整.md`。
- **相似组**：pHash 用 8 段 ×32 位分段索引先并查集分团；阈值 ≤7 时鸽笼原理保证至少有一段完全相同，不会漏候选，完全相同的哈希先并组。单个分段的候选组超过 4,096 张、总检查数超过 2,000 万或组数超过 5,000 时只标记 `truncated` 而不展开。embedding 复核组复用同一批 pHash 候选，再用中心化余弦把组内语义不一致的图片拆开；缺 embedding 的张数记入 `missing`。分组只看范围：临时图片在任何范围都不参与；`scope` 省略或 `library` 时不解析 id，其它范围先解析成 id 集合再过滤分段索引，组内成员不会越出范围。
- **扫描实现**：feature pack mmap + rayon 分块顺序扫描，收集分数后用部分选择取 top-K（不整体排序）。不把 30 万-50 万条向量读进堆，也不逐条查 SQLite。20 万条 1536 维的查询预算由本机基准记录在 `docs/guides/performance.md`。
- **结果缓存**：语义搜索与找相似共用**一个**结果槽（token + 图库代次 + 图库 id），后一次查询覆盖前一次；`get_similarity_page` / `get_similarity_ids` 代次不符时报“相似结果已失效，请重新搜索”。切换图库、编辑、回收站操作、AI 任务结束都会清空它。
- 相似结果只返回 id + 分数，再按页取元数据；不要把向量发给前端。

## 离线与失败

- 服务离线：AI 按钮禁用，设置页显示状态与错误；已有描述/标签/本地相似度不受影响。
- 单张失败：记录失败项，继续下一张；不中断整个任务。
- 模型不兼容：参考图的 embedding 模型或维度不一致时提示“参考图的 embedding 模型或维度不一致，请先统一重算”，不混用不同模型向量。
- 日志不得写入图片内容、完整描述或向量；只记录 id、耗时、状态与错误摘要。

## 应用侧命令与实现位置

客户端、本机 Python 服务端与便携打包脚本都在本仓库；Python 环境与模型文件不随包，由用户放进程序目录。

命令：

| 命令 | 说明 |
| --- | --- |
| `get_ai_status` | 探活、进程状态、health、均值状态；`autoStart` 的拉起也由它触发（任务进度不在这里，看 `list_tasks` 与 `task-progress`） |
| `ai_start_service` / `ai_stop_service` | 托管进程；外部服务也可直接连接 |
| `start_ai_job` / `cancel_ai_job` / `get_ai_job_status` | 已删除；批量任务统一走 `start_batch_task` |
| `ai_describe_image` / `ai_generate_pose` | 单张交互式生成 |
| `semantic_search` | 文本 → embedding → 本地中心化余弦扫描 |
| `find_similar_images` | 参考图找相似：`kind` 为 `phash`/`color4`/`color8`/`embedding`/`pose`，`targetIds` 支持单张或最多 100 张平均（多张只允许 color4/color8/embedding）；可选 `threshold`、`limit`（默认 2000，上限 10000）与 `scope` |
| `start_batch_task` | 统一任务入口：重建缩略图、embedding / pose / description、相似分组（`scope` 可选）；立即返回 `{ status: "started", task }`，`task.phase` 为 `queued`（已排队）或 `preparing`（已拿到资源） |
| `preview_batch_task` | 只读预览：返回 `{ total, willProcess }`（“范围 N 张 / 将处理 M 张”）；`kind` 只接受缩略图与三类 AI 任务，缺模型维度时 `willProcess` 为 `null`。与任务启动共用 `query::PendingFilter` 的同一套待处理条件，异步执行以免阻塞窗口事件循环 |
| `list_tasks` / `cancel_task` | 任务列表与取消；进度通过 `task-progress` / `task-finished` 推送 |
| `get_similarity_groups_result` | 按 taskId 取回相似分组结果 |
| `similarity_group_keeps` | 合并前挑每组保留哪一张：`{ groups, sort }` → 与 `groups` 等长的数组，元素是保留的图片 id（组内没有可见图片时为 null）；只读，排序指标限评分/导入时间/修改时间/像素/大小/文件夹 |
| `get_similarity_page` / `get_similarity_ids` | 相似结果分页与全选（语义搜索与找相似共用） |
| `get_image_pose` | 读取 feature pack 中的 33 点姿势 |

相似分组请求字段：`groupPhash` / `phashThreshold`（0–7）与 `groupEmbedding` / `embeddingThreshold`（0.5–1，默认 0.97）可以单独或同时启用；同时启用时两个条件都满足才归组。只开语义时，pHash 分段索引只用于产生候选，不参与筛选。`scope` 与找相似共用同一套 `TaskScope`，语义是“只在范围内找组”，任务名取范围名。结果结构 `{ phash, embedding, groups, truncated, missing, elapsedMs }`。

实现位置：

- `src-tauri/src/ai/client.rs`：`/v1` 客户端、超时、重试、响应校验。
- `src-tauri/src/ai/process.rs` + `runtime.rs`：Python 启动/探活/退避重启/退出；解析 Python、脚本与模型路径；stdout/stderr 写 `logs/ai-server.log`；保存设置后重启；`find_similar_images` 与结果缓存；相似分组任务（`start_groups_task`）与扫描结果缓存。
- `src-tauri/src/app/paths.rs`：程序根目录、`settings.json`、`logs/`、相对路径与 CLIP 模型目录解析。
- `server/main.py`：`/v1` HTTP 路由、错误映射、health 与 shutdown。
- `server/clip_cache.py`：SigLIP2 图片塔/文本塔独立加载、单塔驻留、空闲卸载。
- `server/pose_engine.py`：MediaPipe Pose 33 点。
- `server/image_io.py`：图片字节解码与 EXIF 方向。
- `src-tauri/src/app/tasks/`：任务总线、资源准入（`concurrency/resources.rs`）、默认排队（FIFO + 路径写优先）、panic 兜底与统一事件；`handle.rs` 放任务句柄，`bus.rs` 放总线与进度泵。
- `src-tauri/src/ai/pack.rs`：追加式 feature pack、包头校验与按偏移切片。
- `src-tauri/src/ai/embedding.rs`：fp16 落盘、均值、mmap 扫描、目标图相似、pHash 候选的 embedding 复核。
- `src-tauri/src/ai/pose.rs`：33 点 f32 存取与 pose 相似度扫描。
- `src-tauri/src/ai/pose_similarity.rs`：姿势相似度与 golden 值测试。
- `src-tauri/src/library/similarity.rs`：常驻视觉索引、pHash/主色相似、pHash 分段分组。
- `src-tauri/src/ai/job.rs`：批量任务、待处理过滤（与预览共用 `PendingFilter`）、取消、失败清单。
- `src-tauri/src/ai/input.rs`：缩略图（embedding/pose）与 1536px WebP（description）输入。
- `src/modules/ai/`：类型、API、服务状态；批量任务与相似分组对话框在 `src/app/`（`BatchTaskDialog.tsx`、`SimilarityGroupsDialog.tsx`，保留规则编辑器是 `KeepSortPicker.tsx`），任务中心在 `src/app/TaskCenterProvider.tsx` / `TaskCapsule.tsx`；`src/app/useSimilarityBrowse.ts`：语义搜索与找相似共用的结果数据源（分页、全选、重试）；`src/modules/library/FindSimilarPanel.tsx`：参考图、指标、阈值、数量与范围（工具栏下拉面板）；`src/shared/similarity.ts`：kind 与中文标签；`src/shared/scope.ts` + `src/shared/components/ScopePicker.tsx`：批量任务、空间优化、找相似与相似分组共用的范围语义与单选框。

限制：

- 任务状态不持久化；关库/退出后重跑会重新计算缺失项。
- 新增 embedding 不会实时更新均值；按用户决定，重开图库才刷新。
- feature pack 采用追加写入，重算会留下旧字节；不做压缩回收（模型切换会让整包作废重算），维护页只显示每个包的条数、已用与垃圾字节。
- 相似模式（语义或找相似）不叠加图库筛选条件；进入后筛选与排序控件禁用。找相似可以显式选择当前视图等范围，范围在进入结果视图前解析。
- 多张参考图只支持主色与 embedding 的平均；pHash 与姿势平均没有意义。缺特征时弹窗置灰，不在查询路径里自动生成。
- 结果缓存只保留最近一次查询；换一次搜索/找相似后旧 token 失效。相似分组结果按 taskId 在内存里最多保留 5 份，关库即清空。
- 相似分组的 pHash 阈值只支持 0-7；更大阈值只能用单张“找相似”。语义复核固定取全库第一名的模型与维度，范围里的图片若属于旧模型会计入 `missing`。
- 常驻视觉索引 20 万行约 46 MB，随图库打开常驻；索引失效后首次查询要重建（见性能文档，真实美术库数字待实测）。
- 被托管的服务进程在运行中退出后只标记失败，不自动重启；需要用户手动启动。
- 客户端串行推理，没有并发设置；批量任务按图片顺序逐张请求。
- 图片塔与文本塔同一时间只驻留一个；切换任务类型需要重新加载（tokenizer 0.9 s + 首次 forward 2.4 s），最后一次使用后空闲 5 分钟自动卸载。塔未加载时 `/v1/embed/*` 返回 503，下一次任务开始前的 ensure 会重新加载。
- 依赖预热让被托管的服务进程常驻约 420 MB；只有整场会话完全不用 AI 时才是净浪费，可在设置里关掉。
- 语义搜索依赖 health 在 `ready=false` 时仍返回图片模型名与维度；外部服务实现必须遵守这一点。
- 描述能力未实现；批量任务中的描述项按缺少能力置灰。
- 设置与日志固定写在程序目录，不从旧 AppData 设置迁移；程序目录不可写时启动失败。
- 便携包只包含 exe、`server/` 与资源清单，不包含 `python_embeded/`、`resources/models/`；不生成 NSIS 安装器。
- 真实服务、真实模型与 WebView2 界面待用户手工验收，清单见[待办](../planning/todo.md)。

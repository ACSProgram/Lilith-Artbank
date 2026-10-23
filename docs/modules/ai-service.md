# AI 服务接口

description、image/text embedding、pose 都通过本机 HTTP 服务完成。图库本体不依赖模型进程；服务离线时相关按钮灰显，浏览、导入、编辑不受影响。

服务实现与 Python 环境不在本仓库。应用只依赖下面的 `/v1` 契约，因此服务端可以复用现有 OnnxRuntimeEngine、另写 Python 包装，或换成别的实现。

## 进程托管

设置项：

```text
ai.baseUrl         默认 http://127.0.0.1:18766
ai.executablePath  可执行文件；为空时只连接外部已启动的服务
ai.port            启动参数与探活端口
ai.autoStart       随应用启动
```

生命周期：

1. 启动后先 `GET /v1/health` 探活。成功则标记在线，不重复拉起。
2. `autoStart` 且 `executablePath` 非空、探活失败时启动进程；等待健康检查最多 60 秒。
3. 端口被其他程序占用且健康检查不是本服务格式 → 标记不可用，提示端口冲突；不尝试杀进程。
4. 崩溃后按需重启，最多 3 次，指数退避；失败写日志并保持离线状态。
5. 退出时优先调用 `POST /v1/shutdown`（可选端点，返回 2xx 即视为已关闭），超时后终止子进程；日志写入应用日志目录，按大小轮转。
6. 服务只绑定 `127.0.0.1`。v1 不做鉴权；`baseUrl` 不是本机地址时在界面给出警告。

不打包 Python 环境是当前决定；未来若打包，只改进程托管与安装布局，不改 `/v1` 契约。

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
- 超时：embedding 60s，pose 60s，description 300s；连接超时 5s。
- 重试：网络错误与 5xx 最多 2 次（250ms、1s 退避）；4xx 不重试。
- 并发：S6 客户端固定串行（1 个在途推理）；模型通常串行，先保证进度、取消与失败清单正确，后续需要时再加并发设置。
- 请求体上限：建议 64 MiB；description 输入应使用降采样预览而不是 100MP 原图。

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
    "textEmbedding": { "ready": true, "model": "siglip2-giant-opt-patch16-384", "dim": 1536 },
    "pose": { "ready": true, "model": "mediapipe-pose" },
    "description": { "ready": false, "model": "" }
  }
}
```

- 未加载的模型 `ready=false`；应用可调用 `/v1/models/ensure` 再试。
- 没有该能力时整个字段可省略。

### POST /v1/models/ensure

按需加载模型，幂等；可能耗时数十秒到数分钟。

```json
{ "capabilities": ["imageEmbedding", "pose"] }
```

响应 `{ "capabilities": { ... } }`，结构同 health。加载失败返回 422/503 并保留旧模型。

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

### POST /v1/embed/text

```json
{ "text": "穿红色斗篷的侧身人物", "model": "siglip2-giant-opt-patch16-384" }
```

响应同 image embedding。服务端负责分词与长度截断；超长文本应截断而不是报错（在 `truncated` 字段说明）。

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

## 应用侧任务

### 任务类型

- `embedding`：为选中/当前视图/全库缺失的图片生成 image embedding。
- `pose`：为选中/当前视图/全库缺失的图片生成 pose。
- `description`：为选中/当前视图/描述为空的图片生成描述；默认只填空，可选覆盖。
- `textSearch`：只做一次 text embedding，然后本地扫描。

### 队列与进度

- 任务有 `jobId`、`kind`、`total`、`completed`、`failed`、`currentImageId`、`message`。
- 进度事件节流 100ms；完成事件带失败清单（imageId + 原因）。
- 取消：停止调度新请求；在途请求允许完成并保存结果；已保存结果不回滚。
- 发送的图片：embedding 与 pose 使用缩略图包里的 512px WebP；description 解码原图（应用 EXIF 方向）后缩到最长边 1536，避免把 100MP 原图直接上传。
- 已完成的 embedding/pose 写入 feature pack 与 `feature_blobs`；描述写入 `images.description`，并设置 `description_source='ai'`、`description_model`。用户手工编辑后 `description_source='user'`。
- 任务状态不持久化；崩溃后已保存结果保留，重跑时默认只处理缺失项。

## 存储

```text
<library>/.artbank/features/
  embedding-<model-slug>-1536-f16.pack    raw little-endian f16 records
  pose-<model-slug>-f32.pack              raw little-endian f32 records
```

- 追加式写入，`feature_blobs` 记录 `(image_id, kind, model, dim, pack_name, pack_offset, pack_length)`；同一图片重算时追加新记录并更新指针，旧字节由 S8 的整理任务回收。
- embedding 磁盘上每条 `dim × 2` 字节（仅存 fp16）；读入后立即转 fp32，均值、点积、范数与余弦全部用 f32 在 CPU 计算。pose 每条 33 × 4 × 4 字节（f32，x/y/z/visibility）。
- 读取向量不整体载入：扫描按 `pack_offset` 排序后 mmap 顺序读，数据库只做一次批量查询；单条读取（查看器 pose）按偏移定位。

## 本地检索

- **均值中心化**：打开图库后在后台扫描当前 model/dim 的全部 image embeddings，求 fp32 均值并保存在内存；会话内不随新增/编辑更新。关闭并重新打开图库刷新。
- **语义搜索**：text → text embedding → `cosine((v - mean), (q - mean))` 与相同 model/dim 的 image embeddings 扫描。比较时同时计算 `dot(v, q)` 与 `dot(v, mean)`，由 `||v||≈1` 还原中心化后的范数，保持精确余弦。
- **相似图片**：目标图片的 image embedding 同样减均值后扫描；没有 embedding 时先调用服务生成。
- **扫描实现**：feature pack mmap + rayon 分块顺序扫描 + 有界 min-heap 取 top-K。不把 30 万-50 万条向量读进堆，也不逐条查 SQLite。20 万条 1536 维的查询预算由本机基准记录在 `docs/guides/performance.md`。
- **姿势相似（S7）**：按原程序 `MediaPipePoseData::similarity` 1:1 移植；扫描 pose pack。原实现源码见 `docs/planning/archive/2026-10-23-S5取消与AI模型调整.md` 的记录路径。
- 语义/姿势排序只返回 id + 分数，再按页取元数据；不要把向量发给前端。

## 离线与失败

- 服务离线：AI 按钮禁用，设置页显示状态与错误；已有描述/标签/本地相似度不受影响。
- 单张失败：记录失败项，继续下一张；不中断整个任务。
- 模型不兼容：语义搜索提示“需要重算 embedding”，不混用不同模型向量。
- 日志不得写入图片内容、完整描述或向量；只记录 id、耗时、状态与错误摘要。

## 当前实现（S6 客户端）

本仓库只实现客户端；服务端与模型环境不在仓库内。

命令：

| 命令 | 说明 |
| --- | --- |
| `get_ai_status` | 探活、进程状态、health、均值状态、当前任务 |
| `ai_ensure_models` | 调用 `/v1/models/ensure` |
| `ai_start_service` / `ai_stop_service` | 托管进程；外部服务也可直接连接 |
| `start_ai_job` / `cancel_ai_job` / `get_ai_job_status` | embedding / pose / description 批量任务 |
| `ai_describe_image` / `ai_generate_pose` | 单张交互式生成 |
| `semantic_search` | 文本 → embedding → 本地中心化余弦扫描 |
| `get_similarity_page` / `get_similarity_ids` | 语义结果分页与全选 |
| `get_image_pose` | 读取 feature pack 中的 33 点姿势 |

实现位置：

- `src-tauri/src/ai/client.rs`：`/v1` 客户端、超时、重试、响应校验。
- `src-tauri/src/ai/process.rs` + `runtime.rs`：进程启动/探活/退避重启/退出；`--port <port>`。
- `src-tauri/src/ai/pack.rs`：追加式 feature pack 与包头校验。
- `src-tauri/src/ai/embedding.rs`：fp16 落盘、均值、mmap 扫描。
- `src-tauri/src/ai/pose.rs`：33 点 f32 存取。
- `src-tauri/src/ai/job.rs`：批量任务、缺失项过滤、取消、失败清单。
- `src-tauri/src/ai/input.rs`：缩略图（embedding/pose）与 1536px WebP（description）输入。
- `src/modules/ai/`：类型、API、服务状态、任务对话框。
- 语义搜索界面在 `src/app/useSemanticSearch.ts` 与图库工具栏编排；结果只传 id，不传向量。

限制：

- 任务状态不持久化；关库/退出后重跑会重新计算缺失项。
- 新增 embedding 不会实时更新均值；按用户决定，重开图库才刷新。
- feature pack 采用追加写入，重算会留下旧字节；压缩/回收留到 S8。
- 语义搜索当前不叠加图库筛选条件；进入语义模式时筛选与排序控件禁用。
- 真实服务、真实模型与 WebView2 界面待用户手工验收。

## 验收

- 用 mock HTTP 服务覆盖成功、400、422、503、超时与重试；任务可取消。
- 真实服务接入后：单张 embedding/pose/description 成功；批量任务进度可见；断网/关服务时 UI 正确降级。
- 描述只填空模式不会覆盖用户手写描述。
- 语义搜索只用同一 model/dim 的向量，并且 image/text 两侧都减去同一均值；结果 id 能正确分页。
- 旧模型（如 1024 维 CLIP）的向量不参与检索；维护页可重算或清理。

# AI 服务接口

description、image/text embedding、pose 都通过本机 HTTP 服务完成。图库本体不依赖模型进程；服务离线时相关按钮灰显，浏览、导入、编辑不受影响。

服务实现与 Python 环境不在本仓库。应用只依赖下面的 `/v1` 契约，因此服务端可以复用现有 OnnxRuntimeEngine、另写 Python 包装，或换成别的实现。

## 进程托管

设置项：

```text
ai.baseUrl         默认 http://127.0.0.1:18766；探活与所有请求都走这里
ai.executablePath  可执行文件；为空时只连接外部已启动的服务
ai.port            启动子进程时传 `--port`；与 baseUrl 的端口需保持一致
ai.autoStart       随应用启动
```

生命周期：

1. 启动后先 `GET /v1/health` 探活。成功则标记在线，不重复拉起。
2. `autoStart` 且 `executablePath` 非空、探活失败时启动进程；等待健康检查最多 60 秒。
3. 端口被其他程序占用且健康检查不是本服务格式 → 标记不可用，提示端口冲突；不尝试杀进程。
4. 启动失败按 1s、2s 退避重试，最多 3 次；仍失败则记录状态与错误并保持离线，由用户在设置页或 AI 任务里手动重启。运行中被托管的进程退出时标记为失败，不自动重启。
5. 退出时优先调用 `POST /v1/shutdown`（可选端点，返回 2xx 即视为已关闭），超时后终止子进程。
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
- 超时：embedding 60s，pose 60s，description 300s，`/v1/models/ensure` 600s；连接超时 5s。
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

- 任务有 `jobId`、`kind`、`total`、`completed`、`failed`、`skipped`、`currentImageId`、`message`。
- 进度事件约 150ms 一次；完成事件带失败清单（imageId + 原因，最多保留 200 条）。
- 取消：停止调度新请求；在途请求允许完成并保存结果；已保存结果不回滚。
- 熔断：连续 5 张失败视为服务不可用，任务以失败结束，避免把整库都记成失败。
- 发送的图片：embedding 与 pose 使用缩略图包里的 512px WebP（缺失或读取失败时回退解码原图）；description 解码原图（应用 EXIF 方向）后缩到最长边 1536，避免把 100MP 原图直接上传。
- 已完成的 embedding/pose 写入 feature pack 与 `feature_blobs`；描述写入 `images.description`，并设置 `description_source='ai'`、`description_model`。用户手工编辑后 `description_source='user'`。
- 任务状态不持久化；崩溃后已保存结果保留，重跑时默认只处理缺失项。

## 存储

```text
<library>/.artbank/features/
  embedding-<model-slug>-1536-f16-<hash8>.pack   raw little-endian f16 records
  pose-<model-slug>-132-f32-<hash8>.pack         raw little-endian f32 records
```

- 文件名由 kind、模型 slug（小写、非字母数字折叠为 `-`、最多 48 字符）、维度、dtype 与模型名 BLAKE3 前 8 位组成，同一模型名始终落到同一个包。
- 包头 32 字节：magic `ARTBFTR1`、version、kind、dtype、dim、record_len、创建时间（14-15 字节保留）；`record_len` 必须等于 `dim × 2`（f16）或 `dim × 4`（f32）。
- 追加式写入，`feature_blobs` 记录 `(image_id, kind, model, dim, pack_name, pack_offset, pack_length)`；同一图片重算时追加新记录并更新指针，旧字节由 S8 的整理任务回收。
- `feature_blobs.dim` 对 embedding 是向量维度，对 pose 是 landmark 个数（33）；pose 包的 `dim` 是 f32 元素数 132（33 × 4），两者不要混用。
- embedding 磁盘上每条 `dim × 2` 字节（仅存 fp16）；读入后立即转 fp32，均值、点积、范数与余弦全部用 f32 在 CPU 计算。pose 每条 33 × 4 × 4 字节（f32，x/y/z/visibility）。
- 读取向量不整体载入：扫描按 `pack_offset` 排序后 mmap 顺序读，数据库只做一次批量查询；单条读取（查看器 pose）按偏移定位。

## 本地检索

相似度有三类数据源：常驻视觉索引（pHash + 4/8 主色）、feature pack（embedding）、feature pack（pose）。三者都只返回 id + 分数，按 token 分页后再回填元数据。

- **常驻视觉索引**（`library/similarity.rs` 的 `VisualIndex`）：载入 `images` 中 `phash`/`colors4`/`colors8` 完整、未删除的行，pHash 存 4×u64，两套主色预转 OKLab（`colors::prepare_palette`），20 万行约 46 MB。用 `count(*) + max(id)` 判断是否仍然有效，导入等新增行会让它重建；编辑、移入/恢复/永久删除、清空回收站经 `invalidate_similarity()` 显式失效。
- **pHash 相似**：目标图 256 位哈希，Hamming 距离升序、同距离按 id 升序；遍历常驻索引做位运算扫描，不再查库。
- **主色相似**：目标图 4/8 色与索引逐项做对称加权 OKLab 距离（每项匹配对方最近色、按自身权重平均，两个方向再取均值），距离升序。
- **均值中心化**：打开图库后在后台扫描当前 model/dim 的全部 image embeddings，求 fp32 均值并保存在内存；会话内不随新增/编辑更新。关闭并重新打开图库刷新。
- **语义搜索**：text → text embedding → `cosine((v - mean), (q - mean))` 与相同 model/dim 的 image embeddings 扫描。比较时同时计算 `dot(v, q)` 与 `dot(v, mean)`，由 `||v||≈1` 还原中心化后的范数，保持精确余弦。
- **相似 embedding**：目标图的 image embedding 同样减均值后扫描。目标图没有 embedding 时直接报错要求先跑 AI 任务，不在查询路径里顺手生成。
- **姿势相似**：按原程序 `MediaPipePoseData::similarity` 1:1 移植（`ai/pose_similarity.rs`），33 点按躯干、四肢 2D/3D、PCA、径向签名、凸包面积加权，镜像后取较大值；每个 pose 包 mmap 一次后顺序切片。原实现源码见 `docs/planning/archive/2026-10-23-S5取消与AI模型调整.md` 的记录路径。
- **全库相似组**：pHash 用 8 段 ×32 位分段索引先并查集分团；阈值 ≤7 时鸽笼原理保证至少有一段完全相同，不会漏候选，完全相同的哈希先并组。单个分段的候选组超过 4,096 张、总检查数超过 2,000 万或组数超过 5,000 时只标记 `truncated` 而不展开。embedding 复核组复用同一批 pHash 候选，再用中心化余弦把组内语义不一致的图片拆开；缺 embedding 的张数记入 `missing`。
- **扫描实现**：feature pack mmap + rayon 分块顺序扫描，收集分数后用部分选择取 top-K（不整体排序）。不把 30 万-50 万条向量读进堆，也不逐条查 SQLite。20 万条 1536 维的查询预算由本机基准记录在 `docs/guides/performance.md`。
- **结果缓存**：语义搜索与找相似共用**一个**结果槽（token + 图库代次 + 图库 id），后一次查询覆盖前一次；`get_similarity_page` / `get_similarity_ids` 代次不符时报“相似结果已失效，请重新搜索”。切换图库、编辑、回收站操作、AI 任务结束都会清空它。
- 相似结果只返回 id + 分数，再按页取元数据；不要把向量发给前端。

## 离线与失败

- 服务离线：AI 按钮禁用，设置页显示状态与错误；已有描述/标签/本地相似度不受影响。
- 单张失败：记录失败项，继续下一张；不中断整个任务。
- 模型不兼容：语义搜索提示“需要重算 embedding”，不混用不同模型向量。
- 日志不得写入图片内容、完整描述或向量；只记录 id、耗时、状态与错误摘要。

## 当前实现（S6 客户端 + S7 本地相似度）

本仓库只实现客户端；服务端与模型环境不在仓库内。

命令：

| 命令 | 说明 |
| --- | --- |
| `get_ai_status` | 探活、进程状态、health、均值状态、当前任务；`autoStart` 的拉起也由它触发 |
| `ai_ensure_models` | 调用 `/v1/models/ensure` |
| `ai_start_service` / `ai_stop_service` | 托管进程；外部服务也可直接连接 |
| `start_ai_job` / `cancel_ai_job` / `get_ai_job_status` | embedding / pose / description 批量任务 |
| `ai_describe_image` / `ai_generate_pose` | 单张交互式生成 |
| `semantic_search` | 文本 → embedding → 本地中心化余弦扫描 |
| `find_similar_images` | 目标图找相似：`kind` 为 `phash`/`color4`/`color8`/`embedding`/`pose`，可选 `threshold`、`limit`（默认 2000，上限 10000） |
| `find_similar_groups` | 全库相似组：`kind` 为 `phash`/`embedding`；`threshold` 是 pHash 阈值（0-7），`similarityThreshold` 是余弦阈值（0.5-1，默认 0.97） |
| `get_similarity_page` / `get_similarity_ids` | 相似结果分页与全选（语义搜索与找相似共用） |
| `get_image_pose` | 读取 feature pack 中的 33 点姿势 |

实现位置：

- `src-tauri/src/ai/client.rs`：`/v1` 客户端、超时、重试、响应校验。
- `src-tauri/src/ai/process.rs` + `runtime.rs`：进程启动/探活/退避重启/退出；`--port <port>`；`find_similar_images` / `find_similar_groups` 与结果缓存。
- `src-tauri/src/ai/pack.rs`：追加式 feature pack、包头校验与按偏移切片。
- `src-tauri/src/ai/embedding.rs`：fp16 落盘、均值、mmap 扫描、目标图相似、pHash 候选的 embedding 复核。
- `src-tauri/src/ai/pose.rs`：33 点 f32 存取与 pose 相似度扫描。
- `src-tauri/src/ai/pose_similarity.rs`：原程序姿势相似度 1:1 移植，附 golden 值测试。
- `src-tauri/src/library/similarity.rs`：常驻视觉索引、pHash/主色相似、全库 pHash 分组。
- `src-tauri/src/ai/job.rs`：批量任务、缺失项过滤、取消、失败清单。
- `src-tauri/src/ai/input.rs`：缩略图（embedding/pose）与 1536px WebP（description）输入。
- `src/modules/ai/`：类型、API、服务状态、任务对话框。
- `src/app/useSimilarityBrowse.ts`：语义搜索与找相似共用的结果数据源（分页、全选、重试）；`src/app/SimilarityGroupsDialog.tsx`：全库相似组扫描与存为集合；`src/shared/similarity.ts`：kind 与中文标签。

限制：

- 任务状态不持久化；关库/退出后重跑会重新计算缺失项。
- 新增 embedding 不会实时更新均值；按用户决定，重开图库才刷新。
- feature pack 采用追加写入，重算会留下旧字节；压缩/回收留到 S8。
- 相似模式（语义或找相似）不叠加图库筛选条件；进入后筛选与排序控件禁用。
- 结果缓存只保留最近一次查询；换一次搜索/找相似后旧 token 失效。
- 全库 pHash 分组的阈值只支持 0-7；更大阈值只能用单张“找相似”。
- 常驻视觉索引 20 万行约 46 MB，随图库打开常驻；索引失效后首次查询要重建（见性能文档，真实美术库数字待实测）。
- 被托管的服务进程在运行中退出后只标记失败，不自动重启；需要用户手动启动。
- 客户端串行推理，没有并发设置；批量任务按图片顺序逐张请求。
- 真实服务、真实模型与 WebView2 界面待用户手工验收。

## 验收

- 用 mock HTTP 服务覆盖成功、400、422、503、超时与重试；任务可取消。
- 真实服务接入后：单张 embedding/pose/description 成功；批量任务进度可见；断网/关服务时 UI 正确降级。
- 描述只填空模式不会覆盖用户手写描述。
- 语义搜索只用同一 model/dim 的向量，并且 image/text 两侧都减去同一均值；结果 id 能正确分页。
- 找相似：`phash`/`color4`/`color8`/`embedding`/`pose` 都能排除目标图并按分数排序；目标图缺数据时给出可行动的提示，而不是返回空列表。
- 全库相似组：20 万条库上不 O(n²) 卡死；`truncated` 与 `missing` 计数正确；embedding 复核能把 pHash 命中但语义不一致的图片拆开。
- 姿势相似度与原程序同输入的分数在浮点误差内一致（golden 值测试覆盖）。
- 旧模型（如 1024 维 CLIP）的向量不参与检索；维护页可重算或清理。

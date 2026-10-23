# 系统架构

Lilith Artbank 是本地优先的大规模美术素材图库。目标规模是几十万张图片与数百 GB 文件；架构的第一原则是**不把全量数据搬进内存或前端**，第二原则是**原图只读、派生数据可重建**。

## 范围

保留原 Lilith 图库本体的能力：导入/去重、文件夹、标签/评分/描述、筛选/排序、集合、回收站、查看器、裁剪/旋转/调尺寸、图像处理。

不做：PinBoard/PasteBoard/ImagePaste、调色盘、色彩测试、截屏小工具、应用内数据迁移、公开版本与法务材料。

AI 能力（description、image/text embedding、pose）统一走本机 HTTP 服务；应用负责服务进程生命周期，但不实现模型与 Python 环境。

## 分层

```text
src/app/                     应用外壳、设置、图库入口、全局通知、跨模块编排
src/modules/library/         图库视图、选择、网格/列表（后续）
src/modules/import/          导入弹窗、进度、结果与失败清单
src/modules/viewer/          查看器（后续）
src/modules/editor/          图像处理（后续）
src/modules/ai/              AI 任务与设置页（后续）
src/shared/                  无领域语义的控件、API 调用、缩略图 URL 与格式化

src-tauri/src/app/           设置、AppState、Tauri 命令适配、窗口生命周期、thumb:// 协议
src-tauri/src/library/       schema、SQLite 连接池、图库生命周期、缩略图 pack、导入仓储、查询与仓储（逐步补齐）
src-tauri/src/import/        BLAKE3、解码/EXIF、扫描、pHash/主色调度、导入后台任务
src-tauri/src/imageops/      缩略图编码、pHash、主色；裁剪/旋转/调尺寸/色彩/LUT/DFT/卷积（后续）
src-tauri/src/ai/            HTTP 客户端、任务队列、服务进程托管（后续）
src-tauri/src/features/      pHash/颜色/向量/姿势相似度（后续）
src-tauri/src/platform/      剪贴板、拖出、Windows 差异集中处（后续）
```

Tauri 命令是薄适配器：解析 DTO、取当前图库、调用领域函数、把错误转成稳定字符串。业务逻辑不写在命令里。

## 应用生命周期

- 单实例：再次启动只聚焦已有窗口，避免两个进程同时写同一图库。
- 窗口使用无边框 + 40px 自绘标题栏；最小化/最大化/关闭由 Rust 命令控制。
- 启动时读取设置与上次图库；上次图库不可用则退回欢迎页并提示，不阻塞启动。
- 打开图库：校验目录、清单与数据库，建立单写 + 多读连接，读取计数；关闭只释放连接，不改文件。
- 导入期间禁止关闭、切换、创建或移除图库；任务完成后恢复。应用退出时已提交批次保留，未完成临时文件在下次打开时清理。
- 移除图库只移除列表项，不删除磁盘内容。真正的文件删除只发生在回收站清空/永久删除。
- AI 服务默认随应用启动（可配置），启动失败不阻塞图库本体；退出时先请求服务优雅关闭。

## 存储布局

```text
<library>/
  .artbank/
    library.json           图库清单（格式、版本、libraryId、名称、创建时间）
    library.db             SQLite 元数据（WAL，FTS5）
    thumbs/0001.pack       追加式缩略图包（段头 16 字节，默认 1 GiB/段）
    features/              embedding / pose 包（后续）
    temp/                  原子写入与编辑中间文件；启动时清理
  <folder>/<content-hash>.<ext>
  recycle/
  output/                  描述导出/回填流水线
```

不变量：

- `content_hash`（BLAKE3，64 位十六进制）全局唯一；文件名即内容哈希，内容不变就不改文件。
- `images.id` 是稳定代理键；标签、集合、回收站与相似度都引用 id。
- 文件夹是 `/` 分隔的图库内相对路径；根目录为空字符串。
- 缩略图与特征都是可重建缓存：丢失只影响显示，不影响原图与元数据。
- 主库不存 embedding/pose 大块；只存它们在 feature pack 中的位置。缩略图同理只存 segment/offset/length。

## 并发模型

- SQLite：一个写连接（`IMMEDIATE` 事务）+ N 个只读连接；WAL + `synchronous=NORMAL`；读连接 `query_only`。
- 写事务只做元数据与指针切换；解码、缩放、编码、哈希都在事务外并行完成。
- 导入：rayon 工作池处理哈希、解码、缩略图、pHash 与主色；单写通道每 64 张或 250ms 批量提交；进度事件节流推送，支持取消与失败清单。
- 前端只持有当前页与选择集；滚动时按需请求下一页。不要在前端维护全库数组。
- 长任务（导入、AI、清理）必须有 job id、进度、取消与失败清单。

## 前端契约

- `src/modules/<module>/api.ts` 是该领域命令的唯一入口；`types.ts` 只放边界 DTO。
- 模块之间不互相导入；跨模块编排放在 `src/app/`。
- 页面组件只做视图、弹窗与局部交互；读取、mutation、busy、错误与请求代次在 controller 中。
- 列表数据使用键集游标（`(sort_key, id)`），不用深 `OFFSET`。
- 缩略图通过本地协议按需加载（`thumb://` / `http://thumb.localhost`），浏览器负责解码与缓存；网格只挂载视口附近项目。

## 性能原则

- 启动：打开库只读清单、schema 版本与计数；不遍历图片、不载入全量元数据。
- 首屏：一次取 150 条元数据；缩略图按可见顺序读取包文件，前端预取前后一屏。
- 查询：常见筛选/排序走索引；文本搜索走 FTS5 trigram，2 字以内回退 `LIKE`。
- 相似度：pHash 常驻为紧凑数组做线性扫描；CLIP/pose 从 mmap 特征包扫描，不逐条查库。
- 所有性能结论必须有本机实测，方法与门槛见 [性能基准](../guides/performance.md)。

## 关键取舍记录

| 主题 | 决策 | 原因 |
| --- | --- | --- |
| 元数据 | SQLite bundled | 20 万行实测首屏 0.12ms、深分页 0.53ms；事务/索引/FTS 现成，不附 DLL |
| 缩略图 | 追加式 `thumbs/*.pack` | 15 万次随机读实测比逐文件快约 16 倍，且避免 20 万小文件 |
| 缩略图格式 | WebP q80（libwebp 静态链接） | 体积约 JPEG 一半，解码时间同级；浏览器原生解码，不用 DDS/WebGPU；无额外 DLL |
| 原图命名 | BLAKE3 内容哈希 | 精确去重、天然不可变；感知相似用独立 256 位 pHash |
| 编辑历史 | 参数化操作链 | 不再保存全分辨率像素副本；预览在降采样图上由 Rust 渲染 |
| AI | 本机 HTTP 服务 + 应用托管进程 | 模型/Python 环境与图库解耦；离线时图库功能不受影响 |
| 旧数据 | 不读 `.data` | 新数据结构独立；迁移脚本后续单独做，应用不背历史格式 |

## 当前实现状态

S0 已实现：设置、图库清单、SQLite schema v1、连接池、图库创建/打开/关闭/移除、窗口命令、应用外壳与图库入口。

S1 前半已实现：WebP 缩略图编码、追加式缩略图包、`thumb://` 协议、应用内 150 张加载基准（用户已确认满足门槛）。

S1 后半已实现：文件/目录/拖放导入、流式 BLAKE3、原子发布、EXIF 方向、pHash、4/8 主色、精确重复与合并、感知重复提示、rayon 工作池、单写批量入库、进度/取消/失败清单、导入界面。1000 张合成图实测 47.7–50.0 张/秒；真实窗口导入界面待用户手工验收。

尚未实现：查询/筛选、查看器、图像处理、AI。具体见 [路线图](../planning/roadmap.md)。

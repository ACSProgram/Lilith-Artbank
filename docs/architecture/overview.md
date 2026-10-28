# 系统架构

Lilith Artbank 是本地优先的大规模美术素材图库。目标规模是几十万张图片与数百 GB 文件；架构的第一原则是**不把全量数据搬进内存或前端**，第二原则是**原图只读、派生数据可重建**。

## 范围

保留原 Lilith 图库本体的能力：导入/去重、多级文件夹、标签/评分/描述、筛选/排序、集合、回收站、操作记录与撤销、查看器、裁剪/旋转/调尺寸。

不做：PinBoard/PasteBoard/ImagePaste、调色盘、色彩测试、截屏小工具、应用内数据迁移、公开版本与法务材料。

AI 能力（description、image/text embedding、pose）统一走本机 HTTP 服务；应用负责服务进程生命周期，但不实现模型与 Python 环境。

## 分层

```text
src/app/                     单库外壳、设置、图库入口、全局通知、跨模块编排
src/modules/library/         文件夹树、图库视图、虚拟网格/列表、筛选、选择、查询控制器、操作记录面板
src/modules/import/          导入弹窗、进度、结果与失败清单
src/modules/viewer/          查看器、几何编辑工具、姿势叠加
src/modules/ai/              AI 状态、任务对话框与设置入口
src/shared/                  无领域语义的控件、确认文案、错误文案、缩略图/原图 URL、剪贴板与查看器会话契约
src/styles/                  tokens.css（浅/深语义颜色变量）与 index.css（组件样式，只引用变量）

src-tauri/src/app/           设置、AppState、Tauri 命令适配、窗口生命周期、thumb:// 与 img:// 协议、任务总线
src-tauri/src/concurrency/   资源表（会话/时间线/内容写/元数据写/AI 服务/文件夹子树路径锁）、图像工作预算、进度计量
src-tauri/src/library/       schema、SQLite 连接池、图库生命周期、文件夹服务、缩略图 pack、导入仓储、键集查询、查看器源与编辑保存、操作记录与撤销、pHash/主色相似索引与分组
src-tauri/src/import/        BLAKE3、解码/EXIF、扫描、pHash/主色调度、导入后台任务
src-tauri/src/imageops/      只做像素：缩略图编码、pHash、主色（含 OKLab 预转换）、几何编辑、压缩编码、拼图
src-tauri/src/platform/      剪贴板等平台差异（文本与图片写入）
src-tauri/src/ai/            HTTP 客户端、任务队列、服务进程托管、feature pack、embedding 检索、姿势相似度
```

Tauri 命令是薄适配器：解析 DTO、取当前图库、调用领域函数、把错误转成稳定字符串。业务逻辑不写在命令里。

## 应用生命周期

- 单实例：再次启动只聚焦已有窗口，避免两个进程同时写同一图库。
- 窗口使用无边框 + 40px 自绘标题栏；最小化/最大化/关闭由 Rust 命令控制。
- 设置 v7 只保存一个 `libraryPath`（`mcp` 字段控制应用内 MCP 服务，`ai.prewarmImports` 控制启动预热）；文件在程序根目录（release 为 exe 旁），不读旧 AppData 设置；启动时尝试打开它，失败则退回欢迎页并提示，不阻塞启动。操作记录不写设置，保留在每个图库自己的 SQLite 里。
- 同一时间只有一个图库会话。打开或创建新库成功后才替换旧会话；关闭只释放连接并清空 `libraryPath`，不改文件（临时文件夹除外：打开、关闭、换库与退出都会清空它）。
- 导入期间禁止关闭、创建或切换图库；任务完成后恢复。应用退出时已提交批次保留，未完成的导入临时文件在下次打开时清理。
- 文件夹的创建/重命名/移动/删除与物理目录同步，和导入、图像保存、回收站操作一样经任务总线的资源表准入：文件夹只占对应子树路径，读图任务只在读原图时持短读锁。
- 应用内没有“移除图库”条目；更换图库即打开另一个目录并替换当前会话。原图文件只在永久删除/清空回收站的记录过期、或用户清空操作记录之后才真正删除。
- AI 服务默认随应用启动（可配置），启动失败不阻塞图库本体；退出时先请求服务优雅关闭。

## 程序目录与运行环境

- 运行环境：Windows + WebView2（系统自带运行时，不随包附带 DLL）。Node 24 与稳定版 Rust 工具链只在开发时需要。
- 设置、日志与相对资源路径都以程序根目录为准（release 为 exe 所在目录，debug 为仓库根目录，可用 `LILITH_ARTBANK_ROOT` 覆盖）；不读旧 AppData 设置，程序目录不可写时启动失败并提示。

```text
Lilith Artbank\
  lilith-artbank.exe
  server\                   随包分发（/v1 服务端）
  python_embeded\           用户提供，不打包
  resources\models\         用户提供，不打包
  settings.json             首次运行生成
  logs\                     lilith-artbank.log / ai-server.log / mcp.log
  mcp-runtime.json          MCP 端口与口令，本地凭据（已 gitignore）
```

`npm run pack:portable` 只产出 exe + `server/` 的便携文件夹，不生成 NSIS 安装器；把 `python_embeded/` 与 `resources/models/` 放进去即可运行。

## 存储布局

```text
<library>/
  .artbank/
    library.json           图库清单（格式、版本、libraryId、名称、创建时间）
    library.db             SQLite 元数据（WAL，FTS5）
    thumbs/0001.pack       追加式缩略图包（段头 16 字节，默认 1 GiB/段）
    features/              embedding（fp16）/ pose（f32）追加式包
    temp/                  原子写入与编辑中间文件；打开图库时清理
    temp/viewer/           大图降采样预览缓存（`<id>-<hash>.webp`，按内容哈希失效）
    undo/<operation-id>/   编辑撤销用的旧原图暂存；记录消失后由打开图库时的清理删除
  .artbank-temp/           临时文件夹（`folders.kind='temp'`，同名冲突时为 .artbank-temp-1）
  <folder>/<content-hash>.<ext>   文件夹就是真实目录，空文件夹也保留
  recycle/
  output/                  描述导出/回填流水线
```

`.artbank`、`recycle`、`output` 与 `.artbank-temp*` 是保留目录，不能作为普通图片文件夹（任何层级）；`.artbank/undo` 由操作记录使用，`.artbank-temp*` 只作为临时文件夹的落地区，关闭、切换图库或退出时清空。

不变量：

- `content_hash`（BLAKE3，64 位十六进制）全局唯一；文件名即内容哈希，内容不变就不改文件。
- `images.id` 是稳定代理键；标签、集合、回收站、操作记录与相似度都引用 id。
- 文件夹是 `/` 分隔的图库内相对路径；根目录为空字符串。路径按 `NOCASE` 唯一，`folders` 中保证每个祖先都有行，磁盘目录与数据库行一一对应；空文件夹保留。
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
- 列表数据使用键集游标（`(sort_key, id)`），不用深 `OFFSET`；一页默认 150，最多 300。
- 浏览命令只经 `src/modules/library/api.ts`；返回行只含列表字段与缩略图版本，不含 embedding/pose。
- 缩略图通过本地协议按需加载（`thumb://` / `http://thumb.localhost`），浏览器负责解码与缓存；网格只挂载视口附近项目。
- 原图与降采样预览通过 `img://`（`http://img.localhost`）按图片 id 提供，URL 带内容哈希做缓存失效；大图预览由 Rust 生成并缓存。
- 选择集只存图片 id；`Ctrl+A` 通过 `query_image_ids` 取当前筛选的全部 id，而不是只选已加载行。
- 复制文件路径与复制图片都在 Rust 侧写系统剪贴板；前端不直接读图库目录。
- 单库外壳由左侧文件夹树 + 主区网格组成；`BrowseState` 提升到工作区，树与网格共享同一份 `filters.folderIds`，树选择即子树筛选。
- 文件夹操作（新建、重命名、移动、删除空文件夹）只经 `src/modules/library/api.ts`；成功后用返回的完整 `FolderInfo[]` 刷新树，不在前端本地改路径。
- 网格/列表拖到树或选择器时使用内部 MIME 传递图片 id；文件夹拖放同样只传 id，由 Rust 校验目标与冲突。
- 样式只有一个颜色来源：`src/styles/tokens.css` 定义浅色（`:root`）与深色（`html[data-theme="dark"]`）两套语义变量，组件样式只引用变量，不写字面色值。主题模式（`light` / `dark` / `system`）由 `src/app/theme.ts` 解析后写到 `html[data-theme]`；查看器、对话框与主界面共用同一套变量，没有“某个界面固定深色”的例外。
- 破坏性操作必须确认：确认框统一经 `useNotices().confirm`，语义化文案在 `src/shared/confirm.ts`（`confirmTrash` / `confirmDelete` / `confirmEmptyTrash`）。`Modal` 负责焦点陷阱、背景滚动锁与 `Esc` 关闭；弹层（排序、筛选、菜单）统一用 `src/shared/components/Popover.tsx` 定位，菜单分组标题用 `MenuSection`。

## 性能原则

- 启动：打开库只读清单、schema 版本与计数；不遍历图片、不载入全量元数据。
- 首屏：一次取 150 条元数据；缩略图按可见顺序读取包文件，前端预取前后一屏。
- 查询：常见筛选/排序走索引；文本搜索走 FTS5 trigram，2 字以内回退 `LIKE`。
- 相似度：pHash 与 4/8 主色常驻为紧凑数组（`library/similarity.rs`）做线性扫描，全库相似对先用 8×32 位分段索引收窄候选；embedding/pose 从 mmap 特征包分块扫描，不整体载入、不逐条查库。向量检索使用打开图库时算出的全库均值做中心化余弦。
- 所有性能结论必须有本机实测，方法与门槛见 [性能基准](../guides/performance.md)。

## 关键取舍记录

| 主题 | 决策 | 原因 |
| --- | --- | --- |
| 元数据 | SQLite bundled | 20 万行实测首屏 0.12ms、深分页 0.53ms；事务/索引/FTS 现成，不附 DLL |
| 缩略图 | 追加式 `thumbs/*.pack` | 15 万次随机读实测比逐文件快约 16 倍，且避免 20 万小文件 |
| 缩略图格式 | WebP q80（libwebp 静态链接） | 体积约 JPEG 一半，解码时间同级；浏览器原生解码，不用 DDS/WebGPU；无额外 DLL |
| 原图命名 | BLAKE3 内容哈希 | 精确去重、天然不可变；感知相似用独立 256 位 pHash |
| 列表显示 | 尺寸/格式/大小/评分/标签/时间 | 不保存原始文件名；原图文件名就是内容哈希，避免导入时维护额外身份 |
| 编辑历史 | 参数化操作链 | 不再保存全分辨率像素副本；预览在降采样图上由 Rust 渲染 |
| AI | 本机 HTTP 服务 + 应用托管进程 | 模型/Python 环境与图库解耦；离线时图库功能不受影响 |
| 向量 | SigLIP2 1536 维 fp16 + mmap | 50 万条约 1.6GB 磁盘，查询不常驻大块内存；均值中心化余弦与原程序一致 |
| 旧数据 | 不读 `.data` | 新数据结构独立；迁移脚本后续单独做，应用不背历史格式 |
| 图库数量 | 单库，设置只存 `libraryPath` | 避免多会话互斥、缓存与 AI 均值状态按库切换的复杂度；换库即关旧开新 |
| 文件夹 | 真实磁盘目录 + `folders` 路径行（NOCASE 唯一） | 用户可在资源管理器直接查看/整理；空文件夹可保留；路径本身就是身份，移动时更新子树 |
| 文件夹筛选 | 选中即包含全部后代 | 与资源管理器树的直觉一致；用路径边界避免 `人物` 误匹配 `人物速写` |
| 主题 | 单套语义变量 + `html[data-theme]`，浅/深共用组件样式 | 避免两套样式表漂移；查看器不再单独维护深色配色，深色下画布比面板再暗一档 |
| 设置字段 | 先入库、后接消费者 | 新增字段与消费它的界面同批落地，避免为同一件事改两次设置版本；只有一种实现的字段（如已删掉的 `detailPanelMode`）不入库 |

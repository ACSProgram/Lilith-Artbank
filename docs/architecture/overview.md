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

src-tauri/src/app/           设置、AppState、Tauri 命令适配、窗口生命周期、thumb:// 与 img:// 协议
src-tauri/src/library/       schema、SQLite 连接池、图库生命周期、文件夹服务、缩略图 pack、导入仓储、键集查询、查看器源与编辑保存、操作记录与撤销、pHash/主色相似索引与分组
src-tauri/src/import/        BLAKE3、解码/EXIF、扫描、pHash/主色调度、导入后台任务
src-tauri/src/imageops/      缩略图编码、pHash、主色（含 OKLab 预转换）、几何编辑；色彩/LUT/DFT/卷积已取消（见 S5 取消记录）
src-tauri/src/platform/      剪贴板等平台差异（文本与图片写入）
src-tauri/src/ai/            HTTP 客户端、任务队列、服务进程托管、feature pack、embedding 检索、姿势相似度
```

Tauri 命令是薄适配器：解析 DTO、取当前图库、调用领域函数、把错误转成稳定字符串。业务逻辑不写在命令里。

## 应用生命周期

- 单实例：再次启动只聚焦已有窗口，避免两个进程同时写同一图库。
- 窗口使用无边框 + 40px 自绘标题栏；最小化/最大化/关闭由 Rust 命令控制。
- 设置 v6 只保存一个 `libraryPath`（`mcp` 字段控制应用内 MCP 服务）；文件在程序根目录（release 为 exe 旁），不读旧 AppData 设置；启动时尝试打开它，失败则退回欢迎页并提示，不阻塞启动。操作记录不写设置，保留在每个图库自己的 SQLite 里。
- 同一时间只有一个图库会话。打开或创建新库成功后才替换旧会话；关闭只释放连接并清空 `libraryPath`，不改文件。
- 导入期间禁止关闭、创建或切换图库；任务完成后恢复。应用退出时已提交批次保留，未完成临时文件在下次打开时清理。
- 文件夹的创建/重命名/移动/删除与物理目录同步，和导入、图像保存、回收站操作共用 `maintenance` 守卫。
- 应用内没有“移除图库”条目；更换图库即打开另一个目录并替换当前会话。原图文件只在永久删除/清空回收站的记录过期、或用户清空操作记录之后才真正删除。
- AI 服务默认随应用启动（可配置），启动失败不阻塞图库本体；退出时先请求服务优雅关闭。

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
  <folder>/<content-hash>.<ext>   文件夹就是真实目录，空文件夹也保留
  recycle/
  output/                  描述导出/回填流水线
```

`.artbank`、`recycle`、`output` 是保留目录，不能作为图片文件夹（任何层级）；`.artbank/undo` 由操作记录使用。

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
| 设置字段 | 先入库、后接消费者 | `thumbFit` / `thumbCacheCount` / `viewerFilmstrip` 已分别在 P1、P3 接上消费者；`sort` / `sortProfiles` 与 P4 一起落地，避免中途再改一次设置版本；只有一种实现的字段（如已删掉的 `detailPanelMode`）不入库 |

## 当前实现状态

S0 已实现：设置、图库清单、SQLite schema（当时为 v1，现为 v5）、连接池、图库创建/打开/关闭、窗口命令、应用外壳与图库入口。

S1 前半已实现：WebP 缩略图编码、追加式缩略图包、`thumb://` 协议、应用内 150 张加载基准（用户已确认满足门槛）。

S1 后半已实现：文件/目录/拖放导入、流式 BLAKE3、原子发布、EXIF 方向、pHash、4/8 主色、精确重复与合并、感知重复提示、rayon 工作池、单写批量入库、进度/取消/失败清单、导入界面。1000 张合成图实测 47.7–50.0 张/秒；真实窗口导入界面待用户手工验收。

S2 已实现：键集查询引擎、基础筛选/排序、筛选计数、文件夹/标签列表、全部匹配 id、系统剪贴板文本、虚拟网格/列表、缩略图占位与重试、选择/键盘/右键菜单、基础筛选面板、导入后刷新。20 万行查询预算全部达标；真实窗口滚动与复制路径待手工验收。拖出到资源管理器与复制图片未在 S2 实现。

S3 已实现：描述/标签/文件夹搜索（当前视图/全库）、时间/大小与存在性筛选、详情与批量编辑、标签重命名/删除、文件夹移动（失败回滚）、集合 CRUD/排序/运算、回收站软删除/恢复/永久删除/清空。普通视图分面计数在预算内；搜索与超大集合计数超 100ms 时筛选面板显示加载态。

S4 已实现：查看器（缩放/平移/全屏/上下张/键盘）、大图与 EXIF 方向图的 Rust WebP 预览、旋转/翻转/裁剪/调尺寸、JPEG/PNG/WebP 输出、新内容发布式保存、复制图片与新命令、可编辑详情侧栏。`img://` 协议按内容哈希提供原图与预览；姿势叠加 UI 在 S6 接入真实 landmark 后可用。OLE 拖出暂缓。真实 Tauri 窗口的 S1–S4 交互待用户手工验收。

S6 已实现：AI 服务进程托管（启动/探活/启动失败退避重试/退出）、`/v1` 客户端与重试、feature pack（embedding fp16 / pose f32）、打开图库计算均值、mmap 中心化余弦语义搜索、embedding/pose/description 批量任务与取消/失败清单、查看器姿势生成与叠加、详情侧栏 AI 描述。真实 AI 服务与 WebView2 界面待手工验收。

2026-10-24 单库与多级文件夹已实现：设置 v2（单一 `libraryPath`）与 v1 迁移、schema v2（`folders.path` NOCASE 唯一索引）、单库生命周期与命令清理、文件夹创建/重命名/移动/删除空文件夹、子树筛选与树计数、导入“平铺/保留源目录”、前端单库外壳与文件夹树/面包屑/选择器/拖放。自动测试覆盖迁移、路径校验、文件夹 CRUD、子树查询与导入层级；真实窗口交互待手工验收。

S7 已实现：目标图找相似（pHash / 4 色 / 8 色 / embedding / pose）、全库 pHash 相似组与 embedding 复核组、结果按 token 分页与全选、右键“找相似”与“相似分组”对话框、从当前结果创建集合。姿势相似度按原程序 1:1 移植并有 golden 值测试；20 万级数字待实测。

2026-10-24 体验整改 P0（基础层）已实现：`tokens.css` 语义变量与 `html[data-theme]` 主题切换（`settings.ui.theme` 支持 `light`/`dark`/`system` 并真正生效）、查看器与主界面统一配色、设置 v3（删掉 `thumbnailCacheMb`，新增缩略图填充/缓存张数/排序/排序方案/胶片带字段）、schema v3（`folders.sort_index`）、`ConfirmDialog` 的 `detail`/`checkbox`、`Modal` 焦点陷阱与滚动锁、`Popover`/`MenuSection`、移入回收站统一确认。

2026-10-24 体验整改 P1（浏览主界面）已实现：侧栏与标题栏去重（关库/换库只留设置入口）、主区头部整块删除、工具栏改两行并把 AI 任务/相似分组收进“任务”菜单、缩略图纯图方格 + `ui.thumbFit` 裁剪/适应切换、网格列数变化按活动项锚定滚动、右键菜单分组与键盘导航、新建集合独立对话框、筛选分面隐藏零值、调试文案清理、缩略图 LRU 缓存、设置改左侧分类导航且固定高度。缩略图基准已在 P2 删除。

2026-10-24 体验整改 P2（任务总线）已实现：`app/tasks.rs` 统一任务总线（write / ai / scan 三条 lane、冲突返回与排队/取消现有）、导入与三种 AI 任务迁移、相似分组改异步扫描任务（pHash 与语义两个独立指标，各自开关与阈值，同时开启为 AND）、缩略图重建、批量文件操作（移动/永久删除/清空回收站）进度与取消、`get_image_capabilities` / `get_feature_summary` 能力置灰；前端新增任务中心胶囊/面板、批量任务对话框，相似分组从批量任务中拆出为独立入口。P2 已通过用户手工验收；提交前删掉了 AI 任务自带的 `AiJob` 状态与 `ai-job-*` 兼容事件（进度只走 `task-progress` / `task-finished`），设置对话框改为 700×560 固定尺寸。

2026-10-24 体验整改 P3（查看器与编辑）已实现：几何操作可组合（旋转/翻转带走裁剪矩形、尺寸按新基底等比换算）、编辑撤销栈（50 步、可读标签、`Ctrl+Z` / `Ctrl+Shift+Z` / `Ctrl+Y`）、保存体积预估（`estimate_edit_size`，500ms 防抖）、查看器胶片带（虚拟横向）、全屏状态提升与标题栏隐藏（F11、系统快捷键同步、Esc 先退全屏）、姿势骨架按屏幕像素恒定粗细且可调不透明度、详情标签点击按范围筛选、保存后就地更新列表行而不重置查询。自动检查全绿，真实窗口观感待用户验收。

2026-10-25 体验整改 P4（文件夹与排序）已实现：内部拖放改为 `shared/useDragSort.ts` 指针事件（图片拖入文件夹、文件夹换父级/同级排序、悬停展开、Esc 取消、边缘自动滚动），`reorder_folders` 与 `folders.sort_index` 持久化文件夹手动顺序；查询排序扩到 7 个指标、支持最多 7 级，单级走键集、多级/文件夹走 id 列表游标（token + offset，最多缓存 2 份），新增 `anchor_id` 与 `nextPageToken`；排序全局一套并写入设置，集合内顺序离开集合时暂存；新增排序弹层与最多 50 个排序方案、列表列头排序与文件夹树排序菜单；schema v4 补修改时间与像素表达式索引。用户已手工验收；20 万行基准实测单级首页 0.66-0.76 ms、6 级首次建 id 列表 213 ms、后续翻页与深页 0.83 ms。详见 [体验整改 P1–P4 实现记录](../planning/archive/2026-10-25-体验整改P1-P4记录.md) 与 [当前阶段状态](../planning/current-handoff.md)。

2026-10-25 图库操作撤销批次 1–3 已实现：schema v5（`operations` / `operation_chunks` / `undo_runs`、`images.purge_operation_id` / `origin_operation_id`）与设置 v5（`history.maxSteps`）、`library/history.rs`（同事务记录、线性撤销、裁剪、启动恢复、释放）、永久删除与清空回收站改为延迟释放标记、`TaskKind::Undo` / `ReleaseGarbage`、工具栏“记录”面板；导入/编辑/移动/文件夹、评分/描述/AI 描述/标签/集合与相似分组记录均已接入。撤销后的记录以灰色 `undone` 保留，可重做（`TaskKind::Redo` / `redo_to_operation`），新的图库写入清空重做分支；编辑与导入使用 `.artbank/undo/<op>/` 暂存原图。详见 [操作撤销实现记录](../planning/archive/2026-10-25-操作撤销实现记录.md) 与 [图库与元数据](../modules/library.md#操作记录与撤销)。

2026-10-25 图片空间优化（智能压缩）两批已实现：`imageops/optimize.rs` 的三档判据、阶梯编码与 EXIF/ICC 搬运；`library/optimize_estimate.rs` 的分层估算与只读报告；schema v6 的 `images.optimized_at`；`library/optimize.rs` 的候选收集、分批编码发布与旧图清理；`HistoryKind::Optimize` 不可撤销记录与 `update_label`；`start_batch_task(kind = "optimize")` 的估算凭证校验；`OptimizeDialog` 的估算 → 确认执行两阶段界面。详见 [维护与批量工作流](../modules/maintenance.md#图片空间优化) 与 [当前阶段状态](../planning/current-handoff.md)。

2026-10-25 图片空间优化第三阶段已实现：长边改 64–65535 自由输入，抽样改 50–2000 离散滑块；估算取结果改为依赖任务原始值并带 5 秒超时重试，进度按阶段推进（统计候选只报百分比，编码样本按张显示，百分比加权不回退）；执行在并行编码内部按张上报，编码与提交前分别显示文案；导入管线新增可选的 `optimize` 参数，在解码后、发布前复用同一套压缩内核，去重语义改为压缩后内容哈希，`optimized_at` 标记导入即压缩的行。详见 [维护与批量工作流](../modules/maintenance.md#图片空间优化)、[导入与缩略图](../modules/import.md) 与 [当前阶段状态](../planning/current-handoff.md)。

2026-10-26 MCP 图库控制批次 1（只读）已实现：设置 v6 的 `mcp` 配置、应用内 stateless Streamable HTTP 服务（`127.0.0.1` + 随机 Bearer 口令 + `mcp-runtime.json`）、同 exe 的 `--mcp-stdio` 小桥、8 个只读工具（图库概况、文件夹、标签、集合、搜索、找相似、详情、拼图）、query/ids/sheet/similarity selector、拼图编号与 128 份/2 小时映射缓存。写入、撤销/重做、导入与询问档确认队列未实现。详见 [MCP 图库控制](../modules/mcp.md) 与 [当前阶段状态](../planning/current-handoff.md)。

尚未实现：S8 的维护与批量工作流（描述导出/回填、孤儿清理、缩略图包压缩、库检查、导出、快捷键）；图像处理已按用户决定取消。具体见 [路线图](../planning/roadmap.md)。

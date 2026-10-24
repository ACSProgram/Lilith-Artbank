# 浏览与检索

S3 在 S2 的键集查询与虚拟网格之上补齐了搜索、扩展筛选、集合、回收站和详情/批量编辑。A–C 把文件夹改为多级树，并把浏览状态提升到工作区，让文件夹树与网格共享同一份筛选。P1 重排了主界面：删掉主区头部、工具栏改成两行、缩略图改成纯图方格并可切“裁剪 / 适应”、右键菜单分组、筛选分面隐藏零值。前端仍不持有全库数组，每次只请求一页；筛选计数只在筛选面板打开时计算。

## 查询 DTO

统一查询 DTO 位于 `src-tauri/src/library/query.rs` 与 `src/modules/library/types.ts`：

```text
LibraryQuery {
  view: "all" | "trash" | "collection",
  collectionId?: number | null,     // view=collection 时必填
  search: string,
  searchScope: "view" | "library",  // 默认只搜当前视图
  filters: {
    folderIds: number[],            // 每个 id 表示包含全部后代的子树；根目录等价全库
    extensions: string[],
    scores: number[],               // 0-5
    tagIds: number[],
    tagMode: "any" | "all",
    importedFrom?: number | null,   // 毫秒时间戳
    importedTo?: number | null,
    sizeMin?: number | null,        // 字节
    sizeMax?: number | null,
    description: "any" | "has" | "missing",
    embedding: "any" | "has" | "missing",
    pose: "any" | "has" | "missing"
  },
  sort: {
    key: "importedAt" | "score" | "size" | "collectionPosition",
    descending: boolean
  },
  cursor?: { sortKey: string, id: number } | null,
  limit?: number | null             // 默认 150，限制 1-300
}
```

- 筛选去重并校验：文件夹最多 256、扩展名最多 32、评分只允许 0-5、标签最多 64。
- 文件夹 id 先解析为路径；每个路径匹配自身与全部后代，使用路径边界（`path = ?` 或 `path LIKE ? || '/%'`），避免 `人物` 命中 `人物速写`。选中的 id 全部失效时查询返回空集；包含根目录时不限制文件夹；多选之间取并集。
- `collectionPosition` 只在集合视图有效；集合视图默认按手工顺序升序。
- 排序键都是整数列，游标 `sortKey` 用十进制字符串传递。
- `ImageRow` 新增 `deletedAt`；回收站行在网格/列表中带“回收站”前缀。
- 返回行只含列表需要的字段，不含 embedding/pose 本体：

```text
id, contentHash, fileExt, folderId, folderPath, filePath,
fileSize, width, height, format, hasAlpha, score,
importedAt, modifiedAt, deletedAt, thumbVersion, thumbWidth, thumbHeight,
tags: string[]
```

- `filePath` 是原图的绝对路径（`<folder>/<content_hash>.<ext>`），用于“复制文件路径”。

## 搜索

搜索覆盖三类字段：描述、标签名、文件夹路径。前端在输入停止 260 ms 后提交查询。

- 描述：3 个字符及以上走 FTS5 trigram（`image_search MATCH`），1-2 个字符回退 `LIKE` 子串匹配；标签名与文件夹路径始终用参数化 `LIKE`。
- `searchScope=view`：在 `all`、`trash` 或当前集合内搜索；`searchScope=library`：忽略当前视图与集合，搜索整个图库（包含回收站），界面临时按“全部”展示，并禁用“集合内顺序”排序。
- 命中条件是描述、标签、文件夹三者的并集，再与筛选条件取交集；不改变标签/文件夹筛选的语义。
- 翻页与计数采用不同策略：
  - 翻页（`SearchMode::Page`）：OR + `EXISTS`，让 `LIMIT 150` 尽早停止；
  - 计数/全选（`SearchMode::Count`）：先一次性 `UNION` 出命中 id 的并集，避免每个候选行重复跑相关子查询。
- 搜索框支持 `Ctrl/Cmd+F` 聚焦、`Esc` 清空并退出输入；工具栏有独立的“当前视图 / 全库”切换。

## 筛选与计数

- 筛选面板覆盖扩展名、评分、标签（任一/全部）、导入时间范围、文件大小范围、有描述/有 embedding/有 pose。文件夹筛选移到左侧文件夹树，不再在面板内重复一套。
- 评分与标签分面按“有内容才显示”渲染：计数为 0 且未被选中的项不渲染，已选中的即使为 0 也保留（否则用户没法取消选择）；整段为 0 时整段不出现。计数尚未返回时按“未过滤”处理（全部渲染），避免打开面板瞬间闪一下。
- 每个分面计数排除该分面自身的筛选条件。文件夹分面在计数时按子树展开：先统计每个文件夹的直接命中数，再沿路径前缀累加到自身与全部祖先；`FilterCounts.folders[].count` 因此与树上的子树总数一致。
- 搜索或集合视图下，计数用单条 `WITH matched AS MATERIALIZED (...)` 语句一次求出命中集，再在五个 `UNION ALL` 分支里分别统计总数/文件夹/扩展名/评分/标签；普通视图直接按分面查询。
- 计数只在筛选面板打开时请求，面板内有 160 ms 防抖与加载态；搜索/集合很大时可能超过 100 ms（见[性能](../guides/performance.md)），不会阻塞网格翻页。
- 标签管理器提供标签搜索、计数、重命名与删除；删除前在确认框显示影响数量。管理器一次只载入图片最多的 500 个标签，搜索在该列表内过滤。

## 命令契约

前端入口：`src/modules/library/api.ts`（`copy_text` 经 `src/shared/clipboard.ts`）。参数使用 camelCase。

### 查询与列表

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `query_images` | `{ query }` | `{ items, nextCursor }` |
| `query_image_ids` | `{ query }` | `number[]`（当前查询的全部 id，最多 100 万） |
| `get_filter_counts` | `{ query }` | `FilterCounts` |
| `get_folders` | — | `FolderInfo[]`（`directImageCount` / `totalImageCount`，按路径排序） |
| `get_tags` | `{ limit? }` | `TagInfo[]`（默认/最多 500，按活跃图片数降序） |
| `search_tags` | `{ query, limit? }` | `TagInfo[]`（按名称子串，最多 200） |
| `copy_text` | `{ text }` | `null`（Rust `arboard` 写系统剪贴板） |

`FilterCounts`：

```text
{
  total,
  folders:     [{ folderId, count }],
  extensions:  [{ value, count }],
  scores:      [{ value, count }],
  tags:        [{ tagId, count }]
}
```

### 详情与元数据

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `get_image_detail` | `{ imageId }` | `ImageDetails` |
| `update_image_score` | `{ imageId, score }` | `null` |
| `set_images_score` | `{ imageIds, score }` | 更新行数 |
| `update_image_description` | `{ imageId, description }` | `null` |
| `set_image_tags` | `{ imageId, tagNames }` | `TagRef[]` |
| `add_tags_to_images` | `{ imageIds, tagNames }` | 新增关系数 |
| `remove_tags_from_images` | `{ imageIds, tagNames }` | 删除关系数 |
| `rename_tag` | `{ tagId, name }` | `null` |
| `delete_tag` | `{ tagId }` | 删除关系数 |
| `move_images_to_folder` | `{ imageIds, folderPath }` | `MoveReport` |

- 描述保存时 `description_source` 固定为 `user`、清空 `description_model`，并由 schema 触发器同步 FTS。
- 标签名忽略大小写去重；每张图最多 64 个标签，标签名最多 64 字符。
- 文件夹移动先 `create_dir_all` 目标目录，再逐张 `rename` 原图并更新 `folder_id`；数据库更新失败时把文件移回原路径并记入失败清单。目标文件夹已存在时直接复用；同一文件夹的图片计入 `skipped`。
- `MoveReport`：`{ moved, skipped, failures: [{ imageId?, path, reason }] }`。
- 文件操作前校验内容哈希为 64 位十六进制、扩展名已规范化、目标目录 canonicalize 后仍位于图库根目录内。

### 回收站

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `trash_images` | `{ imageIds }` | 更新行数 |
| `restore_images` | `{ imageIds }` | 更新行数 |
| `delete_images_permanently` | `{ imageIds }` | `DeleteReport` |
| `empty_trash` | — | `DeleteReport` |

- 软删除只写 `deleted_at`，原图不移动；恢复把 `deleted_at` 置空。
- 永久删除在单个写事务里删除数据库行（标签、集合关系、FTS 由外键/触发器级联），提交后再删除原图文件；文件删除失败写入 `DeleteReport.failures`，数据库删除不回滚。
- 缩略图包是追加式的，永久删除不会回收 pack 内的字节；`thumb://` 在行不存在时返回 404。
- `DeleteReport`：`{ deleted, filesRemoved, failures }`。
- 移入/恢复/永久删除/清空与导入、图库切换共用 `maintenance` 守卫：文件操作期间拒绝导入与生命周期切换。

### 集合

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `get_collections` | — | `CollectionInfo[]` |
| `create_collection` | `{ name }` | `CollectionInfo` |
| `rename_collection` | `{ collectionId, name }` | `null` |
| `delete_collection` | `{ collectionId }` | 删除条目数 |
| `reorder_collections` | `{ collectionIds }` | `null` |
| `add_images_to_collection` | `{ collectionId, imageIds }` | 新增条目数 |
| `remove_images_from_collection` | `{ collectionId, imageIds }` | 移除条目数 |
| `combine_collections` | `{ mode, collectionIds, name }` | `CollectionInfo` |

- `CollectionInfo`：`{ id, name, imageCount, sortOrder, createdAt, updatedAt }`；`imageCount` 只统计未删除图片。
- 加入集合自动追加到末尾；重复加入不改变顺序；集合名唯一（忽略大小写）。
- `combine` 的 `mode` 为 `union` / `intersection` / `difference`，至少两个、最多 32 个集合；结果新建集合，顺序沿用第一个集合的手工顺序，再追加其余集合中新出现的 id。
- 集合引用图片 id，不复制文件；图片内容哈希变化（S4 编辑）不影响集合关系。

## 查询与索引

- 排序统一追加 `id` 作为稳定次序：`ORDER BY <sort_col> <dir>, images.id <dir>`。
- 翻页使用行值比较，禁止深 `OFFSET`：

```sql
-- 降序
WHERE (images.score, images.id) < (?, ?)
-- 升序
WHERE (images.score, images.id) > (?, ?)
```

- 视图条件：`deleted_at IS NULL`（all）、`deleted_at IS NOT NULL`（trash）、集合 `JOIN collection_items`（collection）。
- 文件夹筛选先把选中 id 解析为路径，再用 `images.folder_id IN (SELECT id FROM folders WHERE path = ? OR path LIKE ? ESCAPE '\')` 匹配整棵子树。子树展开只扫描 `folders` 行，代价随文件夹数量而不是图片数量增长；路径的唯一性由 `folders(path COLLATE NOCASE)` 保证。
- 扩展名、评分、标签筛选都用参数化 `IN`；标签 `all` 为每个标签一个 `EXISTS`。
- 时间/大小范围是半开区间之外的闭区间（`>=` / `<=`）；描述/embedding/pose 用 `description <> ''` 或 `EXISTS feature_blobs`。
- 首屏、深分页、文件夹、标签、集合都命中现有索引；20 万行实测见[性能基准](../guides/performance.md)。
- 标签摘要在一页返回后按页内 id 批量查询，避免 N+1；详情侧栏单独读取 4/8 主色。

## 前端模块

```text
src/modules/library/
  types.ts                  边界 DTO（含 FolderInfo）
  api.ts                    Tauri 命令入口 + 浏览器预览 mock
  mock.ts                   浏览器预览用的可变数据集与查询/编辑/文件夹实现
  model.ts                  BrowseState、查询构造、排序选项、格式化、筛选键
  useLibraryBrowse.ts       受控 BrowseState、浏览历史与返回
  useImageQuery.ts          首屏/下一页、代次、查询与翻页状态类型、错误重试
  LibraryView.tsx           视图：搜索/排序/选择/键盘/右键菜单/面包屑
  LibraryContextMenu.tsx    右键菜单（夹在视口内，点击外部或滚动即关闭）
  ViewBar.tsx               全部/回收站标签、集合胶囊、集合管理入口、清空回收站
  FolderTree.tsx            侧栏文件夹树：多选、展开、右键菜单、拖放
  folderTreeModel.ts        树构建、可见行展开、祖先路径、面包屑、包含判断
  FolderDialogs.tsx         文件夹命名/重命名对话框
  dnd.ts                    图片与文件夹拖放的内部 MIME 读写
  VirtualGrid.tsx           自适应列数虚拟网格（支持拖出图片 id）
  VirtualList.tsx           固定行高虚拟列表（支持拖出图片 id）
  virtual.ts                两个虚拟视图共用的滚动句柄类型
  FilterPanel.tsx           范围与存在性筛选（不含文件夹分面）
  Inspector.tsx             单张详情与批量编辑侧栏（文件夹选择器）
  CollectionDialogs.tsx     新建集合（预填名称 + 内容来源）、重命名、集合运算
  CollectionManagerDialog.tsx 集合重命名/删除/排序（不再管新建与运算）
  CollectionPickerDialog.tsx  把选中图片加入现有或新集合
  TagManagerDialog.tsx      标签搜索、重命名、删除
  LibraryThumbnail.tsx      占位、失败与重试（含解码缓存）
```

- 相似结果不经过图库查询：`src/app/useSimilarityBrowse.ts` 提供 `SimilarityBrowseState`（`mode: "semantic" | "similar"`、`token`、`findSimilar`、`loadMore`、`ids`），与 `useImageQuery` 共享 `items/status/error/hasMore` 契约，`LibraryView` 只切换数据源；全库相似组在 `src/app/SimilarityGroupsDialog.tsx`。
- 网格列数由容器宽度和设置里的 `ui.gridThumbPx` 决定；只挂载视口前后各一屏的行。列数变化（窗口缩放、详情面板开关）后用 `useLayoutEffect` 按当前活动项补偿 `scrollTop`，让选中的图片留在原视口位置。
- 列表固定 44px 行高，32px 粘性表头；窄窗口隐藏“导入时间/文件夹”列。
- 缩略图只挂载当前可见 + 前后一屏的单元格；`<img decoding="async">` 指向 `thumb://`。失败显示占位，点击重试会给 URL 加 `retry` 参数绕过失败缓存；缓存命中时挂载后补查 `img.complete`。
- 缩略图方格是纯图：格子高度等于宽度，没有文字标签；分辨率/格式/大小只在 `title` 里显示。`ui.thumbFit` 控制 `cover`（裁剪铺满）与 `contain`（完整显示），切换按钮在工具栏，列表视图下禁用。
- `src/shared/thumbCache.ts` 按 URL 做 LRU（上限 `ui.thumbCacheCount`，默认 1024），记录已加载完成的 `<img>`；命中时 `LibraryThumbnail` 不显示骨架屏。淘汰只丢引用、不清 `src`，避免正在显示的图片变白。
- 浏览器预览（无 Tauri）用 4,000 行可变 mock 数据与 SVG data URL 缩略图；含嵌套与空文件夹，可创建/重命名/移动/删除文件夹、拖放图片与文件夹、子树筛选，也可编辑评分/标签/描述、操作集合与回收站；只用于 UI/布局检查，不代表真实性能。
- 滚动接近已加载末尾时请求下一页；一页返回后追加，不整库载入。**限制**：已加载的页不会被回收，用户一直滚到全库末尾时前端会持有全部行；分页窗口回收留给 S9。

## 视图、选择与键盘

- 主界面分两行工具栏：第一行是返回、视图栏（全部 / 回收站 / 集合胶囊 / `＋ 新建集合` / `⋯` 集合管理）、选择摘要、`导入`、`任务` 菜单（批量任务、相似分组）；第二行是文件夹面包屑、搜索框与搜索范围、语义搜索、筛选、排序指标与方向、网格/列表切换、裁剪/适应切换、详情侧栏开关、刷新。图库名与路径只在侧栏出现一次，标题栏不再重复。
- 视图栏：全部 / 回收站两个标签、集合胶囊、集合管理、清空回收站。切回标签时搜索范围重置为“当前视图”。集合胶囊右键可重命名、左移/右移、另存为新集合、删除集合；`＋ 新建集合` 打开独立对话框（预填 `集合 N`、内容来源可选空集合 / 当前选择 / 当前视图），创建后自动切到该集合并清空选择。
- 左侧文件夹树以“全部图片”为根；单击文件夹即把 `filters.folderIds` 设为该子树并显示在面包屑中。Ctrl/Cmd 单击多选，Shift 单击按可见顺序选择区间；展开/折叠状态只影响树，不进入浏览历史。
- 面包屑条位于视图栏下方，显示当前选中的文件夹；点击层级跳回对应子树，“全部图片”清空文件夹筛选。
- 树右键菜单：新建子文件夹、重命名、移动到…、删除空文件夹、把当前选中的图片移到此文件夹。空文件夹也显示在树中，计数为 0。
- 拖放：网格/列表中的图片可拖到树节点或文件夹选择器；树节点可拖到另一个节点改变层级。图片拖放使用 `application/x-artbank-images` 携带 id，文件夹拖放使用 `application/x-artbank-folder`；位置与冲突由 Rust 校验。
- 浏览历史保存完整的视图/搜索/筛选/排序快照；视图、搜索或文件夹筛选变化都会新增记录（切换排序不单独入栈）。`Ctrl/Cmd+Z` 或工具栏后退按钮回到上一状态。
- 单击选中；Ctrl/Cmd 单击切换；Shift 单击按当前已加载顺序区间选择。
- `Ctrl+A` 调 `query_image_ids` 取当前筛选的全部 id，包含尚未滚动加载的图片，结果在工具栏的选择摘要里显示总数。
- 方向键移动活动项（网格按列数上下移动），Shift+方向键扩展区间；PageUp/PageDown、Home/End 滚动。
- Enter 或双击打开查看器；单击只选中。
- Delete/Backspace：普通视图移入回收站，回收站视图永久删除选中项。
- 右键菜单按分组排布：打开 / 复制（路径、内容哈希、图片）/ 整理（加入集合、移出当前集合）/ 找相似（pHash、主色 8 色、主色 4 色、语义、姿势；多选时整组置灰，回收站图片不提供）/ 危险操作（移入回收站、恢复、永久删除）/ 选择（全部匹配项）。菜单弹出即聚焦第一项，↑↓ 在可点项间移动，Esc 关闭；只在用户滚轮或窗口尺寸变化时关闭，程序化滚动（列数变化补偿）不会误关。
- 移入回收站必须确认，且只有一个收口点：右键菜单、Delete 键、详情侧栏按钮、查看器删除都调用 `LibraryView` 的 `trashSelected`，确认文案来自 `src/shared/confirm.ts` 的 `confirmTrash`（“把 N 张图片移入回收站？”，可恢复所以不标红）。永久删除与清空回收站分别走 `confirmDelete` / `confirmEmptyTrash`（标红）。
- 详情侧栏是挤压式右栏（不覆盖画布），宽度随窗口收窄；没有选中图片时显示占位提示，选中单张显示详情，多选显示批量编辑。单击只选中，不再自动打开侧栏；侧栏由工具栏按钮开关，打开状态在会话内保持。
- 侧栏、标题栏都不再放“关闭图库”与“更换图库”；换库与关库都在设置 → 图库里。
- 复制路径走 Rust `copy_text`（`arboard`），浏览器预览回退到 clipboard API / `execCommand`。
- 拖出到资源管理器（Windows OLE）仍未实现；复制图片走右键菜单的“复制 · 图片”。

## 找相似与相似分组

- 入口：网格/列表右键“找相似”只对单张图片可用，多选时整组置灰；工具栏「任务 → 相似分组…」扫描全库。各项按目标图实际具备的特征置灰并给出原因。
- 找相似直接进入相似模式：结果覆盖当前视图，不叠加筛选与搜索；工具栏显示“相似于 #id · pHash · N 张结果”，`Ctrl+A` 经 `get_similarity_ids` 取全部结果 id，退出按钮清空结果源。编辑、回收站操作、导入完成会让结果过期并自动退出。
- 相似分组对话框：pHash（差异上限 0–7）与语义（相似度下限 0.50–1.00）是两个独立指标，各有开关；同时开启时两个条件都满足才归组。扫描走任务总线，有阶段进度、可取消，关掉对话框后进度仍在右下角任务胶囊里，再打开能接回同一任务与结果。结果按缩略图预览展示，可以把每组各存一个集合、全部合并成一个集合，或一次创建最多 100 个集合；集合名与现有集合重名时自动加后缀。
- 相似结果只以 token 留在 Rust 侧，前端拿 id 与该页行数据；接口与阈值细节见 [AI 服务](ai-service.md)。

## 详情与批量编辑

- 详情：缩略图、评分星级、标签（自动补全）、描述编辑、文件夹移动、4/8 主色、尺寸/格式/大小/导入时间、复制哈希/路径、移入回收站/恢复/永久删除、加入/移出集合。
- 批量：统一评分、批量加/删标签、移动文件夹或根目录、回收站操作、加入/移出集合。
- 编辑成功后刷新当前页与分面选项；批量结果会提示失败清单。文件夹移动成功不再重复弹出二次提示。
- 长任务（文件夹移动、永久删除、清空回收站）走任务总线的 write lane：有百分比进度、可取消；永久删除与清空回收站按 400 张一块先提交数据库删行事务、再删文件，取消后已完成部分保留。
- 工具栏语义搜索在 AI 离线或全库没有 embedding 时置灰并给出原因；筛选面板的“有 …”在对应特征全库为 0 时置灰。

## 请求竞态与刷新

- `useImageQuery` 为每次首屏请求递增代次；过期结果直接丢弃，不覆盖新视图。
- 翻页请求与首屏共享代次；筛选/排序/刷新变化后旧页不会追加到新结果。
- 首屏失败显示整页错误与重试；翻页失败保留已加载页，在列表底部提供重试。
- 排序变化会重新从第一页加载并清空滚动位置，但保留图片选择；搜索/筛选变化会清空选择。
- 导入结束会递增 `refreshToken`，重新查询第一页并刷新图库计数；详情编辑递增本地 `dataVersion`。
- 切换图库时 `LibraryView` 以图库 id 为 key 重新挂载，避免复用上一库的筛选与选择。

## 性能

20 万行合成库、4 个读连接、开发档实测（同一台机器；不同轮次受系统负载影响约 2 倍）。下表全部来自 20 文件夹数据集；10,221 个文件夹库上的子树页、`get_folders` 与子树分面计数见[性能基准](../guides/performance.md)的“多级文件夹与子树”一节，不在两处重复维护数字：

| 路径 | p50 | p95 | 预算 |
| --- | --- | --- | --- |
| 首屏 150（导入时间降序） | 0.70 ms | 1.31 ms | ≤50 ms |
| 深分页 150（score/id 游标） | 2.59 ms | 5.36 ms | ≤50 ms |
| 文件夹 150 | 1.55 ms | 3.74 ms | ≤50 ms |
| 标签 150 | 5.65 ms | 7.87 ms | ≤50 ms |
| 集合 150（手工顺序） | 1.65 ms | 2.53 ms | ≤50 ms |
| 搜索 150（FTS trigram） | 8.78 ms | 14.15 ms | ≤50 ms |
| 搜索 150（2 字 LIKE） | 1.40 ms | 4.93 ms | ≤50 ms |
| 搜索 150（标签名） | 5.78 ms | 11.46 ms | ≤50 ms |
| 搜索 150（文件夹路径） | 7.12 ms | 9.32 ms | ≤50 ms |
| 筛选分面计数（普通视图） | 69.54 ms | 96.66 ms | ≤100 ms |
| 筛选分面计数（FTS 搜索） | 132.56 ms | 144.38 ms | 超预算，面板显示加载态 |
| 筛选分面计数（标签/文件夹搜索） | 99.86 / 79.85 ms | 107.71 / 109.96 ms | 约在预算边缘 |
| 筛选分面计数（2 字搜索） | 825.88 ms | 878.44 ms | 超预算，面板显示加载态 |
| 筛选分面计数（20 万条集合） | 778.83 ms | 819.69 ms | 超预算，面板显示加载态 |
| Ctrl+A 取 20 万个 id | 41.36 ms | — | 未设硬门槛 |

- 2 字搜索必须全表扫描 `description`，无法使用 trigram 索引；20 万条集合的计数需要扫描集合命中集并聚合标签关系。两者都只在筛选面板打开时计算，并有加载态。
- 搜索/集合分面计数使用单条 `MATERIALIZED` CTE，比逐分面重复求命中集快数倍；但 20 万级命中仍会超过 100 ms。普通视图计数仍在预算内。
- 浏览器预览（724×712 CSS，4,000 行 mock）验证了 DOM 有界与全部 S3 交互；真实 WebView2 的 20 万行滚动帧率、150 张缩略图加载与真实美术样本仍需手工验收。

## 尚未实现

- 列表列头点击排序、列隐藏；当前用工具栏排序。
- 拖出到资源管理器（Windows OLE）；复制图片到剪贴板已在 S4 实现。
- 相似度作为筛选条件与排序键：S7 只做了“找相似”结果视图与全库相似组，筛选面板与列表排序没有接 pHash/主色/embedding/pose 分数。
- 原始文件名：当前不入库；如需按文件名识别，需要 schema 变更与导入记录。
- 已加载页的窗口回收；长时间滚动到 20 万末尾时前端内存会随已加载行增长。
- 长任务进度/取消、已移出当前筛选的图片仍可能保持选中。

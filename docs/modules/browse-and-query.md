# 浏览与检索

S3 在 S2 的键集查询与虚拟网格之上补齐了搜索、扩展筛选、集合、回收站和详情/批量编辑。A–C 把文件夹改为多级树，并把浏览状态提升到工作区，让文件夹树与网格共享同一份筛选。P1 重排了主界面：删掉主区头部、工具栏改成两行、缩略图改成纯图方格并可切“裁剪 / 适应”、右键菜单分组、筛选分面隐藏零值。P4 把内部拖放改为指针事件实现，补上文件夹手动排序与图片多级排序，并用 id 列表游标支撑多级/文件夹排序的跨页。操作记录批次 1–3 在工具栏加了“记录”面板，可撤销回收站、删除、导入、编辑、移动、文件夹、评分、描述、标签、集合与相似分组操作；撤销成功的记录保留为灰色可重做。前端仍不持有全库数组，每次只请求一页；筛选计数只在筛选面板打开时计算。

## 查询 DTO

统一查询 DTO 位于 `src-tauri/src/library/query.rs` 与 `src/modules/library/types.ts`：

```text
LibraryQuery {
  view: "all" | "trash" | "collection" | "temp",
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
  sort: SortSpec[],                 // 1-7 级，同一指标不可重复
  cursor?: { sortValues: string[], id: number } | null,
  pageToken?: string | null,        // id 列表游标：token:offset
  anchorId?: number | null,         // 从该图片的排序位置开始取页（包含它）
  limit?: number | null             // 默认 150，限制 1-300
}

SortSpec {
  key: "importedAt" | "modifiedAt" | "score" | "size"
     | "pixels" | "folder" | "collectionPosition",
  descending: boolean
}
```

- 筛选去重并校验：文件夹最多 256、扩展名最多 32、评分只允许 0-5、标签最多 64。
- 文件夹 id 先解析为路径；每个路径匹配自身与全部后代，使用路径边界（`path = ?` 或 `path LIKE ? || '/%'`），避免 `人物` 命中 `人物速写`。选中的 id 全部失效时查询返回空集；包含根目录时不限制文件夹；多选之间取并集。
- `collectionPosition` 只在集合视图有效；全库搜索时取不到集合位置，因此也拒绝。`folder`、`pixels` 在所有视图可用。
- 排序值由 Rust 按级转换为十进制字符串放进 `sortValues`；像素尺寸用 `width * height`，文件夹用 `folders.path COLLATE NOCASE`。
- 单级且不是文件夹的排序使用键集游标；多级、文件夹排序或带 `anchorId` 时返回 `nextPageToken`。同一时刻只缓存最新 2 份 id 列表，游标自带查询指纹；任何改变图片集合、排序字段或筛选字段的写入都会让缓存整体失效（`AppState::mutate` 与导入收尾负责），旧 token 再被使用会返回“排序结果已过期”，前端同时通过刷新令牌重新取第一页。
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
- `searchScope=view`：在 `all`、`trash`、`temp` 或当前集合内搜索；`searchScope=library`：忽略当前视图与集合，搜索整个图库（包含回收站，不含临时文件夹），界面临时按“全部”展示，并禁用“集合内顺序”排序。
- 命中条件是描述、标签、文件夹三者的并集，再与筛选条件取交集；不改变标签/文件夹筛选的语义。
- 翻页与计数采用不同策略：
  - 翻页（`SearchMode::Page`）：OR + `EXISTS`，让 `LIMIT 150` 尽早停止；
  - 计数/全选（`SearchMode::Count`）：先一次性 `UNION` 出命中 id 的并集，避免每个候选行重复跑相关子查询。
- 搜索框支持 `Ctrl/Cmd+F` 聚焦、`Esc` 清空并退出输入；同一个搜索框内可切换“匹配 / 语义”两种模式。匹配模式下有独立的“当前视图 / 全库”范围切换；语义模式按 Enter 或输入框右侧按钮提交，结果始终来自全库。

## 筛选与计数

- 筛选面板覆盖扩展名、评分、标签（任一/全部）、导入时间范围、文件大小范围、有描述/有 embedding/有 pose。文件夹筛选移到左侧文件夹树，不再在面板内重复一套。
- 评分与标签分面按“有内容才显示”渲染：计数为 0 且未被选中的项不渲染，已选中的即使为 0 也保留（否则用户没法取消选择）；整段为 0 时整段不出现。计数尚未返回时按“未过滤”处理（全部渲染），避免打开面板瞬间闪一下。
- 每个分面计数排除该分面自身的筛选条件。文件夹分面在计数时按子树展开：先统计每个文件夹的直接命中数，再沿路径前缀累加到自身与全部祖先；`FilterCounts.folders[].count` 因此与树上的子树总数一致。
- 搜索或集合视图下，计数用单条 `WITH matched AS MATERIALIZED (...)` 语句一次求出命中集，再在五个 `UNION ALL` 分支里分别统计总数/文件夹/扩展名/评分/标签；普通视图直接按分面查询。
- 计数只在筛选面板打开时请求，面板内有 160 ms 防抖与加载态；搜索/集合很大时可能超过 100 ms（见[性能](../guides/performance.md)），不会阻塞网格翻页。
- 标签管理器提供标签搜索、计数、重命名与删除；删除前在确认框显示影响数量。管理器一次只载入图片最多的 500 个标签，搜索在该列表内过滤。

## 按标签筛选

详情侧栏的标签 chip 可以直接当筛选入口，不需要新后端能力：动作与范围都映射到现有的 `filters.tagIds` 与视图字段（`model.ts` 的 `tagFilterState`）。

- 动作：`apply` 用这个标签替换标签条件；`append` 在当前视图里叠加/取消这一个标签；`remove` 取消它。Ctrl/Cmd 点击 chip 等于 `append`。
- 范围：`view` 保留当前的视图、搜索与其它筛选；`folder` 只保留当前文件夹子树；`collection` 只保留当前集合；`library` 清空文件夹与集合、在整个图库内筛选。后三者会清空搜索与其它筛选条件，并把排序按新视图换算（离开集合时退回默认排序）。
- 菜单里“当前文件夹/当前集合”两项在没有对应范围时置灰，并说明原因；已生效的标签 chip 显示选中态，菜单第一项变成“取消该标签筛选”。
- 切换后保留原来的图片选择：新查询返回时的第一页如果包含它，滚动到它；不在第一页时用锚点（`useImageQuery.jumpTo`，发送该图 id，Rust 按它的排序位置取一页并包含它）跳过去。锚点走 id 列表通道，因此集合内顺序也可用。取消标签会放宽结果，无法确认目标图仍在结果里，因此这种情况按常规清空选择。
- 筛选后选择不会因为 `selectionResetSignature` 变化被清空：`LibraryView` 用一个一次性标志把这次变化排除在外。

## 命令契约

前端入口：`src/modules/library/api.ts`（`copy_text` 经 `src/shared/clipboard.ts`）。参数使用 camelCase。

### 查询与列表

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `query_images` | `{ query }` | `{ items, nextCursor, nextPageToken }`（两者至多一个非空） |
| `query_image_ids` | `{ query }` | `number[]`（当前查询的全部 id，最多 100 万） |
| `get_filter_counts` | `{ query }` | `FilterCounts` |
| `get_folders` | — | `FolderInfo[]`（`directImageCount` / `totalImageCount`，按路径排序，含 `sortIndex`） |
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

### 文件夹

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `reorder_folders` | `{ folderIds, parentId, sortIndex }` | `FolderInfo[]` |

- 只接受同一父文件夹下的子文件夹；按给定 id 顺序插入到 `sortIndex`，事务内重新编号同级 `sort_index`，返回完整列表。
- 新建文件夹与移动文件夹追加到同级末尾。

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
- 评分、描述、标签设置/增删/改名/删除都写入一条操作记录；撤销恢复旧评分/描述/标签关系，删除标签的撤销用原 id 重建标签与关系。
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
- 永久删除只接受回收站里的图片（整批中有一张不在回收站就拒绝），与清空回收站一样只写 `purge_operation_id` 待释放标记；这些行不再出现在回收站、普通视图与全库搜索中。真正删行删文件由记录过期或清空操作记录后的 `ReleaseGarbage` 任务执行。
- 缩略图包是追加式的，永久删除不会回收 pack 内的字节；`thumb://` 在行被真正释放后返回 404。
- `DeleteReport`：`{ deleted, filesRemoved, deferred, failures }`；批次 1 的永久删除/清空固定 `deferred=true`、`filesRemoved=0`。
- 移入/恢复/永久删除/清空与导入、图库切换共用 `maintenance` 守卫：文件操作期间拒绝导入与生命周期切换。
- 确认文案（`src/shared/confirm.ts`）只描述后果：文件不会立刻释放，清空操作记录后不能再撤销。
- 临时图片不属于回收站流程：`trash_images` / `restore_images` / 永久删除都拒绝临时图片；清空临时文件夹由图库生命周期触发，见 [图库与元数据](library.md#临时文件夹)。

### 临时文件夹

- 视图栏在“回收站”旁有“临时”标签，显示 `tempCount`；数量为 0 时仍可进入，网格提示没有图片。
- `view=temp` 只返回 `folders.kind='temp'` 下的图片；其它视图、全库搜索、文件夹树、标签列表与集合计数都排除临时图片。临时图片仍可被显式 id 查询与相似扫描访问。
- 临时视图里切到“全库”搜索时按全库规则走：结果不含临时图片，界面按“全部”展示。
- 临时视图禁用图片拖出与新建集合，面包屑固定显示“临时”；右键菜单只保留打开、复制与相似，详情侧栏隐藏文件夹移动、集合与回收站操作。查看器的编辑工具只读。
- 临时视图可直接导入：导入对话框固定平铺、只跳过重复、不压缩，完成后图片进入临时文件夹，完成页提示关闭时会清空。
- 批量任务与找相似的 `TaskScope` 新增 `temp`；范围选择器显示“临时文件夹 / N 张”。

### 操作记录

- 第一行工具栏“记录”按钮打开 `OperationHistoryPanel`，与“任务”按钮并排；有任务在运行时按钮置灰，悬停提示正在运行的任务。
- 面板按时间倒序列出 `HistoryPage.entries`，显示类型文案、时间、影响图片数；`undoFailed` 显示错误摘要，重做失败的 `undone` 记录也显示错误并允许再次重试。列表随任务完成后的 `dataVersion` 自动刷新。
- 撤销后的记录不删除，而是变灰留在列表顶部；“当前状态”分隔线标出可重做区与已应用区的边界。选中灰色记录后按钮变为“重做到这条”，确认后调用 `redo_to_operation`；选中已应用记录则仍调用 `undo_to_operation`。两种操作都只统计真正会变化的记录数与影响数。
- 撤销/重做开始后面板保持打开并显示任务进度，完成后刷新网格、文件夹、计数与查看器；新的图库写入会删除灰色记录，列表回到新的时间线。
- 面板底部显示待释放图片数量/字节与“保留最近 N 步”，并提供“清空记录”；清空会立即释放全部待删除文件且不能再撤销或重做。
- 批次 1–3 已产生回收站、删除、导入、编辑、移动、文件夹、评分、描述/AI 描述、标签与集合记录；批量操作只记一条。缩略图重建、embedding/pose 生成与相似扫描本身不进入记录。

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
| `create_collections_from_groups` | `{ groups, baseName }` | `CollectionInfo[]`（一次事务创建全部相似组集合） |

- `CollectionInfo`：`{ id, name, imageCount, sortOrder, createdAt, updatedAt }`；`imageCount` 只统计未删除图片。
- 加入集合自动追加到末尾；重复加入不改变顺序；集合名唯一（忽略大小写）。
- `combine` 的 `mode` 为 `union` / `intersection` / `difference`，至少两个、最多 32 个集合；结果新建集合，顺序沿用第一个集合的手工顺序，再追加其余集合中新出现的 id。
- 集合引用图片 id，不复制文件；图片内容哈希变化（S4 编辑）不影响集合关系。
- 所有集合写命令都产生一条操作记录；`create_collections_from_groups` 用一次事务创建全部集合与条目，只记一条，撤销时整批删除、重做时按原 id 重建。重名时后端按 `名称 1`、`名称 1-2` 规则生成。

## 查询与索引

- 排序统一追加 `id` 作为稳定次序，方向跟随最后一级；多级排序逐级比较，前级相等才比较后级。
- 单级（导入时间 / 修改时间 / 评分 / 大小 / 像素 / 集合内顺序）走键集游标，没有深 `OFFSET`：

```sql
-- 单级降序
WHERE (images.score, images.id) < (?, ?)
-- 单级升序
WHERE (images.score, images.id) > (?, ?)
```

- 多级或文件夹排序没有可直接用于行值比较的复合索引，改走 id 列表通道：首次按完整排序取有序 id（最多 20 万条量级），按 `token:offset` 切片返回；后续页只做切片，不重复排序。缓存最多 2 份，进程内过期即丢弃。
- 锚点查询（`anchorId`）也走 id 列表，从目标图片的排序位置开始取一页并包含它；若它已被筛选排除，就从它原本的位置开始返回后续匹配行，不会把不匹配的行插进结果。

- 视图条件：`deleted_at IS NULL`（all）、`deleted_at IS NOT NULL`（trash）、集合 `JOIN collection_items`（collection）。
- 文件夹筛选先把选中 id 解析为路径，再用 `images.folder_id IN (SELECT id FROM folders WHERE path = ? OR path LIKE ? ESCAPE '\')` 匹配整棵子树。子树展开只扫描 `folders` 行，代价随文件夹数量而不是图片数量增长；路径的唯一性由 `folders(path COLLATE NOCASE)` 保证。
- 扩展名、评分、标签筛选都用参数化 `IN`；标签 `all` 为每个标签一个 `EXISTS`。
- 时间/大小范围是半开区间之外的闭区间（`>=` / `<=`）；描述/embedding/pose 用 `description <> ''` 或 `EXISTS feature_blobs`。
- 首屏、深分页、文件夹、标签、集合都命中现有索引；20 万行实测见[性能基准](../guides/performance.md)。
- 标签摘要在一页返回后按页内 id 批量查询，避免 N+1；详情侧栏单独读取 4/8 主色。

## 排序

- 排序是全局一套（`SortSpec[]`），叠加到全部 / 回收站 / 集合 / 文件夹 / 搜索；写入 `settings.ui.sort` 并在下次启动时恢复。排序变化保留已选图片、清空滚动位置并从第一页重新加载。
- `collectionPosition` 只用于集合视图：离开集合时把整套排序暂存到 `BrowseState.collectionSort`，回到集合再恢复；离开期间用去掉集合顺序后的排序。没有集合顺序可暂存时（普通视图下改了排序）沿用新的全局排序。
- 工具栏排序按钮显示“排序 · 第一级 ↓ + 第二级 ↓（+N）”摘要；弹层内每级可改指标、切方向、上移/下移/删除，最多 7 级且指标不可重复。不可用指标置灰（集合内顺序只在集合视图）。排序旁不再有单独的升降序快捷按钮。
- 排序方案保存在 `settings.ui.sortProfiles`，最多 50 个，名称不区分大小写唯一；可另存、覆盖当前方案、删除。方案不按视图自动套用，切换后由用户手动选择。
- 列表视图的列头可直接改排序：第一级同列再点切换方向，点其它列改为按该列排序（文件夹与集合顺序默认升序，其余默认降序）。
- 相似 / 语义结果固定按相似度，不参与图库排序；此模式下排序按钮置灰，退出结果视图后沿用进入前的全局排序。
- 文件夹树有自己的排序菜单（手动 / 名称 / 图片数含子文件夹 / 图片数直接，可升降序），只影响树内同级顺序，状态存 `localStorage`；只有“手动”模式允许拖动排序。

## 前端模块

```text
src/modules/library/
  types.ts                  边界 DTO（含 FolderInfo / SortSpec / QueryCursor）
  api.ts                    Tauri 命令入口 + 浏览器预览 mock
  mock.ts                   浏览器预览用的可变数据集与查询/编辑/文件夹实现
  model.ts                  BrowseState、查询构造、全局排序与集合顺序暂存、筛选键、标签筛选换算
  useLibraryBrowse.ts       受控 BrowseState、初始排序、浏览历史与返回
  useImageQuery.ts          首屏/下一页、代次、锚点跳转、pageToken 与就地替换行、错误重试
  LibraryView.tsx           视图：搜索/排序/选择/键盘/右键菜单/面包屑
  SortMenu.tsx              排序弹层：多级编辑、方向、排序方案
  LibraryContextMenu.tsx    右键菜单（夹在视口内，点击外部或滚动即关闭）
  FindSimilarPanel.tsx      找相似面板：参考图、指标、最低相似度、结果数量、范围
  ViewBar.tsx               全部/回收站标签、集合胶囊、集合管理入口、清空回收站
  FolderTree.tsx            侧栏文件夹树：多选、展开、右键菜单、排序菜单、指针拖放
  folderTreeModel.ts        树构建与排序、可见行展开、祖先路径、面包屑、包含判断
  FolderDialogs.tsx         文件夹命名/重命名对话框
  VirtualGrid.tsx           自适应列数虚拟网格（支持开始拖动图片）
  VirtualList.tsx           固定行高虚拟列表（列头排序、开始拖动图片）
  virtual.ts                两个虚拟视图共用的滚动句柄类型
  FilterPanel.tsx           范围与存在性筛选（不含文件夹分面）
  Inspector.tsx             单张详情与批量编辑侧栏（文件夹选择器、标签点击筛选菜单）
  CollectionDialogs.tsx     新建集合（预填名称 + 内容来源）、重命名、集合运算
  CollectionManagerDialog.tsx 集合重命名/删除/排序（不再管新建与运算）
  CollectionPickerDialog.tsx  把选中图片加入现有或新集合
  TagManagerDialog.tsx      标签搜索、重命名、删除
  LibraryThumbnail.tsx      占位、失败与重试（含解码缓存）

src/shared/
  scope.ts                  范围语义（ScopeChoice / TaskScope）与可用性、数量文案
  components/ScopePicker.tsx 范围单选框（批量任务、空间优化、找相似共用）
  useDragSort.ts            指针事件拖放控制器（图片与文件夹共用）
```

- 相似结果不经过图库查询：`src/app/useSimilarityBrowse.ts` 提供 `SimilarityBrowseState`（`mode: "semantic" | "similar"`、`targetIds`、`scopeLabel`、`token`、`findSimilar`、`loadMore`、`ids`），与 `useImageQuery` 共享 `items/status/error/hasMore` 契约，`LibraryView` 只切换数据源；参考图、指标/阈值/数量/范围面板在 `src/modules/library/FindSimilarPanel.tsx`；范围语义与单选框在 `src/shared/scope.ts`、`src/shared/components/ScopePicker.tsx`；全库相似组在 `src/app/SimilarityGroupsDialog.tsx`。
- 网格列数由容器宽度和设置里的 `ui.gridThumbPx` 决定；只挂载视口前后各一屏的行。列数变化（窗口缩放、详情面板开关）后用 `useLayoutEffect` 按当前活动项补偿 `scrollTop`，让选中的图片留在原视口位置。
- 列表固定 44px 行高，32px 粘性表头；窄窗口隐藏“导入时间/文件夹”列。
- 缩略图只挂载当前可见 + 前后一屏的单元格；`<img decoding="async">` 指向 `thumb://`。失败显示占位，点击重试会给 URL 加 `retry` 参数绕过失败缓存；缓存命中时挂载后补查 `img.complete`。
- 缩略图方格是纯图：格子高度等于宽度，没有文字标签；分辨率/格式/大小只在 `title` 里显示。`ui.thumbFit` 控制 `cover`（裁剪铺满）与 `contain`（完整显示），切换按钮在工具栏，列表视图下禁用。
- `src/shared/thumbCache.ts` 按 URL 做 LRU（上限 `ui.thumbCacheCount`，默认 1024），记录已加载完成的 `<img>`；命中时 `LibraryThumbnail` 不显示骨架屏。淘汰只丢引用、不清 `src`，避免正在显示的图片变白。
- 浏览器预览（无 Tauri）用 4,000 行可变 mock 数据与 SVG data URL 缩略图；含嵌套与空文件夹，可创建/重命名/移动/删除文件夹、拖放图片与文件夹、子树筛选，也可编辑评分/标签/描述、操作集合与回收站；只用于 UI/布局检查，不代表真实性能。
- 滚动接近已加载末尾时请求下一页；一页返回后追加，不整库载入。**限制**：已加载的页不会被回收，用户一直滚到全库末尾时前端会持有全部行；分页窗口回收留给 S9。

## 视图、选择与键盘

- 主界面分两行工具栏：第一行是返回、视图栏（全部 / 回收站 / 临时 / 集合胶囊 / `＋ 新建集合` / `⋯` 集合管理）、选择摘要、`导入`、`任务` 菜单（批量任务、相似分组）、`记录`（操作记录与撤销）；第二行是文件夹面包屑、统一搜索框（匹配/语义 + 全库/视图）、筛选、排序、网格/列表切换、裁剪/适应切换、详情侧栏开关、刷新。图库名与路径只在侧栏出现一次，标题栏不再重复。
- 视图栏：全部 / 回收站 / 临时三个标签、集合胶囊、集合管理、清空回收站。在视图之间切换保留搜索文本与“当前视图 / 全库”范围；集合顺序随视图切换暂存/恢复，其它排序级别保持不变。临时视图不显示“新建集合”。集合胶囊右键可重命名、左移/右移、另存为新集合、删除集合；`＋ 新建集合` 打开独立对话框（预填 `集合 N`、内容来源可选空集合 / 当前选择 / 当前视图），创建后自动切到该集合并清空选择。
- 左侧文件夹树以“全部图片”为根；单击文件夹即把 `filters.folderIds` 设为该子树并显示在面包屑中。Ctrl/Cmd 单击多选，Shift 单击按可见顺序选择区间；展开/折叠状态只影响树，不进入浏览历史。
- 面包屑条位于视图栏下方，显示当前选中的文件夹；点击层级跳回对应子树，“全部图片”清空文件夹筛选。
- 树右键菜单：新建子文件夹、重命名、移动到…、删除空文件夹、把当前选中的图片移到此文件夹。空文件夹也显示在树中，计数为 0。
- 拖放使用指针事件（`shared/useDragSort.ts`），不依赖 HTML5 DnD，因此在开启原生拖放的 WebView2 里也能工作：
  - 网格/列表里按住图片拖到文件夹行中间 = 移入该文件夹，列表/树的拖放目标与图片集合由 `LibraryWorkspace` 统一提交；
  - 文件夹只能拖到另一个文件夹（换父级）或同级行上下 1/4（排序），拖到自身或后代时拒绝并提示；
  - 悬停文件夹 600ms 自动展开；靠近树或网格边缘自动滚动；Esc 取消；拖动结束后紧跟的一次 click 被忽略，避免改变选择；
  - 从资源管理器拖文件进窗口仍走 Tauri 原生拖放，打开导入对话框并预填当前单选文件夹。
- 浏览历史保存完整的视图/搜索/筛选/排序快照；视图、搜索或文件夹筛选变化都会新增记录（切换排序不单独入栈）。`Ctrl/Cmd+Z` 或工具栏后退按钮回到上一状态。
- 单击选中；Ctrl/Cmd 单击切换；Shift 单击按当前已加载顺序区间选择。
- `Ctrl+A` 调 `query_image_ids` 取当前筛选的全部 id，包含尚未滚动加载的图片，结果在工具栏的选择摘要里显示总数。
- 方向键移动活动项（网格按列数上下移动），Shift+方向键扩展区间；PageUp/PageDown、Home/End 滚动。
- Enter 或双击打开查看器；单击只选中。
- Delete/Backspace：普通视图移入回收站，回收站视图永久删除选中项，临时视图只提示临时图片会在关闭或切换图库时清空。
- 右键菜单按分组排布：打开 / 复制（路径、内容哈希、图片）/ 整理（加入集合、移出当前集合；临时视图隐藏）/ 相似（单张“设为相似参考”，多选“设为相似参考（平均）”；回收站图片置灰，平均参考最多 100 张）/ 危险操作（移入回收站、恢复、永久删除；临时视图隐藏）/ 选择（全部匹配项）。菜单弹出即聚焦第一项，↑↓ 在可点项间移动，Esc 关闭；只在用户滚轮或窗口尺寸变化时关闭，程序化滚动（列数变化补偿）不会误关。
- 移入回收站必须确认，且只有一个收口点：右键菜单、Delete 键、详情侧栏按钮、查看器删除都调用 `LibraryView` 的 `trashSelected`，确认文案来自 `src/shared/confirm.ts` 的 `confirmTrash`（“把 N 张图片移入回收站？”，可恢复所以不标红）。永久删除与清空回收站分别走 `confirmDelete` / `confirmEmptyTrash`（标红，说明文件延迟释放）；撤销、重做与清空操作记录走 `confirmUndoTo` / `confirmRedoTo` / `confirmClearHistory`。
- 详情侧栏是挤压式右栏（不覆盖画布），宽度随窗口收窄；没有选中图片时显示占位提示，选中单张显示详情，多选显示批量编辑。单击只选中，不再自动打开侧栏；侧栏由工具栏按钮开关，打开状态在会话内保持。
- 侧栏、标题栏都不再放“关闭图库”与“更换图库”；换库与关库都在设置 → 图库里。
- 临时视图的详情侧栏仍可改评分、标签、描述与 AI 描述，但不显示文件夹移动、加入/移出集合与回收站操作，并提示“临时图片关闭、切换图库或退出应用时会自动清空”。
- 复制路径走 Rust `copy_text`（`arboard`），浏览器预览回退到 clipboard API / `execCommand`。
- 拖出到资源管理器（Windows OLE）仍未实现；复制图片走右键菜单的“复制 · 图片”。

## 找相似与相似分组

- 参考图：网格/列表右键“设为相似参考”把当前选择保存为参考图；多选时是“设为相似参考（平均）”，只提供主色 4 色、主色 8 色和语义三种平均指标。设置后不会自动弹窗，只提示到工具栏“相似”打开面板。参考图在同一图库会话内保留，相似结果条的“调整”也能改指标、阈值、数量和范围。回收站图片不能作为参考图，平均参考最多 100 张。
- “相似”面板与筛选面板同为工具栏下拉面板，顶部显示参考图缩略图与数量；单张参考支持 pHash、主色 4/8 色、语义 embedding 与姿势，平均参考只支持主色 4/8 色与语义。缺对应特征时指标置灰并说明需要先在批量任务里生成，不在面板里自动计算。
- 阈值统一为“最低相似度”滑杆（0–100%，越大越严格，可勾“不限”）：pHash 映射到 0–64 距离（不限时传 256），主色映射到 0–1 OKLab 距离，语义与姿势映射到 -1–1 相似度。结果数量默认 2000，范围 1–10000。
- 范围与批量任务共用同一套 `TaskScope`：当前视图（默认）、当前选择、当前文件夹（可选是否包含子文件夹）、当前集合、临时文件夹、整个图库。当前选择全都在参考图里时该项置灰；相似结果视图里的“当前视图”按进入相似结果前的图库视图计算。范围单选框抽在共享 `ScopePicker`，批量任务与图片空间优化也复用。
- 后端按范围收窄扫描：全库不解析 id；其它范围先在 Rust 侧解析成 id 集合再取交集。主色与 pHash 仍在常驻视觉索引上扫描，embedding/pose 只读取范围内的向量，不退回 O(n²)。
- 结果仍走统一的相似结果槽：结果覆盖当前视图，不叠加筛选与搜索；结果条显示“相似于文件名 / N 张参考图 · 指标 · 范围 · N 张结果”，平均参考的“平均”由指标名体现，点击文件名打开第一张参考图；`Ctrl+A` 经 `get_similarity_ids` 取全部结果 id，绿色“退出相似结果”清空结果源。编辑、回收站操作、导入完成会让结果过期并自动退出。
- 工具栏「任务 → 相似分组…」仍是独立的扫描全库入口；与相似面板的参考图状态互不影响。
- 语义搜索首次提交时文本塔可能尚未加载，加载期间结果区提示“首次搜索需要先加载文本模型”。
- 相似分组对话框：pHash（差异上限 0–7）与语义（相似度下限 0.50–1.00）是两个独立指标，各有开关；同时开启时两个条件都满足才归组。扫描走任务总线，有阶段进度、可取消，关掉对话框后进度仍在右下角任务胶囊里，再打开能接回同一任务与结果。结果按缩略图预览展示，可以把每组各存一个集合、全部合并成一个集合，或一次创建最多 100 个集合；保存走 `create_collections_from_groups` 一次事务提交，只产生一条操作记录；集合名与现有集合重名时由后端自动加后缀。
- 相似结果只以 token 留在 Rust 侧，前端拿 id 与该页行数据；接口与阈值细节见 [AI 服务](ai-service.md)。

## 详情与批量编辑

- 详情：缩略图、评分星级、标签（自动补全）、描述编辑、文件夹移动、4/8 主色、尺寸/格式/大小/导入时间、复制哈希/路径、移入回收站/恢复/永久删除、加入/移出集合。
- 批量：统一评分、批量加/删标签、移动文件夹或根目录、回收站操作、加入/移出集合。
- 编辑成功后刷新当前页与分面选项；批量结果会提示失败清单。文件夹移动成功不再重复弹出二次提示。
- 长任务（文件夹移动、永久删除、清空回收站、撤销、释放）走任务总线的 write lane：有百分比进度与取消；永久删除与清空回收站只在一段事务里写待释放标记，释放任务按 400 张一块先提交数据库删行事务、再删文件，取消后已完成部分保留。单条记录的撤销与释放不可取消，多条记录的撤销在记录之间检查取消。
- 搜索框的“语义”模式在 AI 离线或全库没有 embedding 时置灰并给出原因；筛选面板的“有 …”在对应特征全库为 0 时置灰。

## 请求竞态与刷新

- `useImageQuery` 为每次首屏请求递增代次；过期结果直接丢弃，不覆盖新视图。
- 翻页请求与首屏共享代次；筛选/排序/刷新变化后旧页不会追加到新结果。
- 首屏失败显示整页错误与重试；翻页失败保留已加载页，在列表底部提供重试。
- 排序变化会重新从第一页加载并清空滚动位置，但保留图片选择；搜索/筛选变化会清空选择（按标签筛选除外，见上）。
- 导入结束会递增 `refreshToken`，重新查询第一页并刷新图库计数；详情编辑递增本地 `dataVersion`。
- 查看器保存编辑不重置查询：只按 `ViewerSessionUpdate` 就地替换已加载行的内容哈希/扩展名/体积/宽高/格式/alpha/`thumbVersion`（相似结果视图走 `SimilarityBrowseState.patchItem`），排序、滚动位置与深分页上下文都保留；分面计数与文件夹计数不随之刷新。
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
- P4 的排序路径在 20 万行上实测：单级排序首页 0.66-0.76 ms；6 级排序首次建 id 列表 213 ms（预算 400 ms）；后续翻页与 offset 199 850 的深页都是 0.83 ms（预算 60 ms）；文件夹单级排序走同一条 id 列表通道，42 ms。数字与口径见[性能基准](../guides/performance.md)。
- 浏览器预览（724×712 CSS，4,000 行 mock）验证了 DOM 有界与全部 S3 交互；真实 WebView2 的 20 万行滚动帧率、150 张缩略图加载与真实美术样本仍需手工验收。

## 尚未实现

- 列表列头只做了点击排序，列显示/隐藏仍未实现。
- 拖出到资源管理器（Windows OLE）；复制图片到剪贴板已在 S4 实现。
- 相似度作为筛选条件与排序键：S7 只做了“找相似”结果视图与全库相似组，筛选面板与列表排序没有接 pHash/主色/embedding/pose 分数。
- 原始文件名：当前不入库；如需按文件名识别，需要 schema 变更与导入记录。
- 已加载页的窗口回收；长时间滚动到 20 万末尾时前端内存会随已加载行增长。
- 长任务进度/取消、已移出当前筛选的图片仍可能保持选中。

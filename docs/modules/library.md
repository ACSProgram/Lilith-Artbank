# 图库与元数据

图库是一个目录加上 `.artbank/` 数据区。应用侧由 `AppState` 持有唯一的当前图库会话，元数据由 SQLite 管理。本文件记录当前已实现的图库契约，包括单库设置 v3、schema v3 与多级文件夹；AI 任务与 feature pack 见 [AI 服务](ai-service.md)。

## 图库清单

`.artbank/library.json`：

```json
{
  "format": "lilith-artbank",
  "formatVersion": 1,
  "libraryId": "uuid",
  "name": "参考",
  "createdAt": 1760000000000
}
```

- `format` 与 `formatVersion` 不匹配时拒绝打开，不做自动升级。
- `libraryId` 必须与数据库 `meta.libraryId` 一致。
- 目录里已有 `.artbank` 但清单/数据库缺失或损坏时，`inspect_library_path` 返回 `invalid`；不会静默重建。
- 空目录可以直接初始化为新图库；非空但没有 `.artbank` 的目录也按空目录处理（不会动用户已有文件）。

## 数据库

路径：`.artbank/library.db`。打开参数：

```sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = -65536;
PRAGMA mmap_size = 268435456;
```

- 写连接只有一条；读连接默认 4 条，`query_only=ON`。
- schema 版本写在 `PRAGMA user_version` 与 `meta.schemaVersion`。当前为 3；版本高于程序支持值时拒绝打开。
- v1 → v2 迁移只新增 `folders(path COLLATE NOCASE)` 唯一索引。迁移前检查是否存在仅大小写不同的重复路径；有冲突则拒绝打开并保留 v1，不做自动改名或合并。
- v2 → v3 迁移给 `folders` 加 `sort_index INTEGER NOT NULL DEFAULT 0`，并按 `path`（NOCASE）顺序编号，升级后文件夹的显示顺序不变。v1 库按 v1→v2→v3 链式升级。

### 表

| 表 | 作用 |
| --- | --- |
| `meta` | `appFormat`、`schemaVersion`、`libraryId`、`createdAt` |
| `folders` | 图库内文件夹；`path` 为 `/` 分隔相对路径，根目录为 `''`；每个祖先都有行，路径 NOCASE 唯一；`sort_index` 为手动排序位 |
| `images` | 图片元数据与缩略图指针 |
| `tags` / `image_tags` | 规范化标签与多对多关系 |
| `collections` / `collection_items` | 手工集合与顺序 |
| `thumb_segments` | 缩略图包分段；0 号为占位段 |
| `feature_blobs` | embedding / pose 在 feature pack 中的位置 |
| `image_search` | FTS5 external-content 表，索引 `images.description` |

`images` 关键字段：

```text
id                 稳定代理键
content_hash       BLAKE3 十六进制（64 字符），UNIQUE
file_ext           小写、无点
folder_id          folders.id
file_size / width / height / format / has_alpha
phash              32 字节（256 位感知哈希）
colors4            4 色 × (r,g,b,weight) = 16 字节
colors8            8 色 × (r,g,b,weight) = 32 字节
score              0-5
description        用户可编辑文本；FTS 索引
description_source user / ai / import
description_model  AI 生成描述时的模型名；手工/导入为空
imported_at / modified_at / deleted_at
thumb_segment / thumb_offset / thumb_length / thumb_format
thumb_width / thumb_height / thumb_version
```

- `deleted_at IS NULL` 表示正常；非空表示回收站。软删除不移动原图；永久删除先删数据库行，再删原图文件。
- `thumb_segment = 0` 表示还没有缩略图；`thumb_version` 用于 URL 缓存失效。缩略图包追加写入，永久删除图片不会回收包内字节。
- `feature_blobs` 只存位置与模型/维度，不存向量本体；避免主库膨胀。
- FTS5 使用 `trigram` tokenizer，支持中文子串；查询少于 3 个字符时回退 `LIKE`（S3 实现）。
- 描述、标签、回收站、集合关系与 FTS 索引由 schema 触发器/外键维护；应用不手工写 FTS 表。
- 不保存原始文件名；原图文件名就是 `<content_hash>.<ext>`。列表显示尺寸、格式、大小、评分、标签与时间，不以文件名作为标识。

### 索引

- `images(deleted_at, score DESC, id DESC)`：评分首屏。
- `images(deleted_at, imported_at DESC, id DESC)`：时间排序。
- `images(deleted_at, file_size DESC, id DESC)`：大小排序。
- `images(folder_id, deleted_at, imported_at DESC, id DESC)`：文件夹页。
- `folders(path COLLATE NOCASE)` UNIQUE：路径解析、重名检查与 v2 迁移。
- `images(deleted_at) WHERE deleted_at IS NOT NULL`：回收站。
- `image_tags(tag_id, image_id)`：标签反查。
- `collection_items(collection_id, position)`：集合顺序。

## 设置

设置文件由 Tauri `app_config_dir` 下的 `settings.json` 保存，原子替换（临时文件 + rename）。字段：

```text
settingsVersion    当前为 3
libraryPath        当前图库绝对路径；null 表示未打开。由后端维护
ai                 { baseUrl, executablePath, port, autoStart }
performance        { readerConnections, workerThreads }
ui                 { theme, gridThumbPx, thumbFit, thumbCacheCount,
                     sort, sortProfiles, viewerFilmstrip }
```

- 设置页提交不会覆盖 `libraryPath`；它只由创建/打开/关闭图库更新。
- v1 的 `libraries[]` 与 `lastLibraryId` 在读取时迁移为单个 `libraryPath`：优先 `lastLibraryId` 对应条目，否则取第一个条目。迁移后不再写回旧字段。
- v2 → v3：删掉从未被任何代码读取的 `performance.thumbnailCacheMb`；新增的 `ui` 字段用默认值补齐（`thumbFit=cover`、`thumbCacheCount=1024`、`sort=[{key:"importedAt",descending:true}]`、`sortProfiles=[]`、`viewerFilmstrip=true`）；`theme` 增加 `system` 并成为默认值。`ai`、`performance`、`theme`、`gridThumbPx` 的旧值原样保留。
- P0 曾加过 `ui.detailPanelMode`（`overlay`/`push`）；P1 确认详情面板只有“挤压画布 + 列数变化锚定”一种实现，字段已删除。旧设置文件里的 `detailPanelMode` 由 serde 忽略，不报错、不回写。
- 图库路径必须绝对；保存时去掉尾部路径分隔符，但保留盘符根（`G:\`）。
- `readerConnections` 1-16；`workerThreads` 为空表示自动。
- `theme` 只能是 `light` / `dark` / `system`；`gridThumbPx` 96-512；`thumbFit` 只能是 `cover` / `contain`；`thumbCacheCount` 256-8192。
- `sort` 至少一级、最多 7 级，指标只能取自 `importedAt` / `modifiedAt` / `score` / `size` / `pixels` / `folder` / `collectionPosition`，同一指标不能重复；`sortProfiles` 最多 50 个，名称非空且不区分大小写唯一，每个方案的 `sort` 同样校验。
- 生效范围：`theme` 由 `src/app/theme.ts` 解析并写到 `html[data-theme]`，`system` 跟随 `prefers-color-scheme` 实时切换；`thumbFit` 由浏览工具栏的“裁剪 / 适应”切换并即时写入；`thumbCacheCount` 由 `src/shared/thumbCache.ts` 作为缩略图 LRU 上限消费（改动后立即收缩）；`sort` / `sortProfiles`（P4）、`viewerFilmstrip`（P3）仍只有校验，消费者随各自批次接入。
- 设置文件损坏时备份为 `settings.bad-<时间>.json` 并使用默认值，不阻塞启动。

## 命令契约

前端统一经 `src/app/api.ts` 调用图库生命周期命令；导入命令经 `src/modules/import/api.ts` 调用，契约见 [导入与缩略图](import.md)；浏览/查询命令经 `src/modules/library/api.ts` 调用，契约见 [浏览与检索](browse-and-query.md)。参数使用 camelCase。

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `get_settings` | — | `Settings` |
| `save_settings` | `{ settings }` | `Settings` |
| `inspect_library_path` | `{ path }` | `LibraryInspection` |
| `create_library` | `{ name, path }` | `LibraryInfo`（创建并打开） |
| `open_library_path` | `{ path }` | `LibraryInfo`（只打开已存在的图库） |
| `close_library` | — | `null` |
| `get_library_status` | — | `LibraryInfo \| null` |
| `rename_library` | `{ name }` | `LibraryInfo`（只改名称，不改目录） |
| `query_images` | `{ query }` | `{ items, nextCursor }` |
| `query_image_ids` | `{ query }` | `number[]` |
| `get_filter_counts` | `{ query }` | `FilterCounts` |
| `get_folders` | — | `FolderInfo[]` |
| `create_folder` | `{ path }` | `FolderInfo[]`（完整列表） |
| `rename_folder` | `{ folderId, name }` | `FolderInfo[]` |
| `move_folder` | `{ folderId, parentId }` | `FolderInfo[]` |
| `delete_folder` | `{ folderId }` | `FolderInfo[]` |
| `get_tags` | `{ limit? }` | `TagInfo[]` |
| `search_tags` | `{ query, limit? }` | `TagInfo[]` |
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
| `trash_images` / `restore_images` | `{ imageIds }` | 更新行数 |
| `delete_images_permanently` | `{ imageIds }` | `DeleteReport` |
| `empty_trash` | — | `DeleteReport` |
| `get_collections` | — | `CollectionInfo[]` |
| `create_collection` | `{ name }` | `CollectionInfo` |
| `rename_collection` | `{ collectionId, name }` | `null` |
| `delete_collection` | `{ collectionId }` | 删除条目数 |
| `reorder_collections` | `{ collectionIds }` | `null` |
| `add_images_to_collection` / `remove_images_from_collection` | `{ collectionId, imageIds }` | 变更条目数 |
| `combine_collections` | `{ mode, collectionIds, name }` | `CollectionInfo` |
| `get_viewer_source` | `{ imageId }` | `ViewerSource` |
| `save_image_edit` | `{ request }` | `EditResult` |
| `get_image_pose` | `{ imageId }` | `PoseData \| null`（feature pack） |
| `copy_image` | `{ imageId }` | `null` |
| AI 命令（status/ensure/任务/描述/pose/语义搜索） | 见 [AI 服务](ai-service.md) | — |
| `window_toggle_fullscreen` | — | `boolean` |
| `copy_text` | `{ text }` | `null` |
| `window_minimize` / `window_toggle_maximize` / `window_is_maximized` / `window_close` | — | 窗口控制 |
| `get_app_info` | — | `{ version, settingsPath, logPath }` |
| `reveal_path` | `{ path }` | `null`（在系统文件管理器里选中该文件，或打开该目录） |

`LibraryInspection`：

```text
{ kind: "library", manifest }
{ kind: "empty" }
{ kind: "invalid", reason }
```

`LibraryInfo`：`id`、`name`、`path`、`imageCount`、`trashCount`、`schemaVersion`、`openedAt`。

`FolderInfo`：`{ id, path, directImageCount, totalImageCount }`。`directImageCount` 只统计直接位于该文件夹的未删除图片，`totalImageCount` 包含全部后代；根目录的 `path` 为空字符串。

S3 的编辑/集合/回收站命令细节、返回结构与失败语义见 [浏览与检索](browse-and-query.md)。要点：

- 元数据写入都走单写连接；批量操作按 400 个 id 分块。
- 图片移动逐张 `rename` + 更新 `folder_id`，数据库失败时回滚文件；永久删除先删行再删文件。
- 文件操作（图片移动、文件夹操作、永久删除、清空回收站）与导入、图库切换共用 `maintenance` 守卫；长任务当前没有进度事件与取消。
- 集合只引用图片 id；集合运算新建集合，不修改源集合。

## 文件夹

文件夹路径既是显示层级也是磁盘位置：`folders.path` 的每一段对应图库根目录下的同名目录。

- 路径规范：`/` 分隔、无空段、不能以 `/` 开头或结尾；每段拒绝 Windows 非法字符 `<>:"\|?*`、控制字符、结尾空格/句点、保留设备名（`CON`、`PRN`、`AUX`、`NUL`、`COM1-9`、`LPT1-9` 等）。
- 任何层级都拒绝 `.artbank`、`recycle`、`output`；根目录不能创建、重命名、移动或删除。
- 新建可以给出多级路径；不存在的祖先会同时创建物理目录与数据库行。路径按 NOCASE 判重，大小写不同也视为已存在。
- 重命名只改最后一段，保留父路径；仅大小写变化时通过临时目录中转（Windows 不能直接改大小写）。移动接收目标父文件夹 id，拒绝移动到自身或后代。
- 重命名/移动会先改物理目录，再在单写事务里更新整个子树的 `path`；数据库失败时把目录移回。目标位置已有同名文件夹或磁盘目录时拒绝。
- 删除只允许空文件夹：没有子文件夹、没有任何图片（包括回收站图片）、物理目录为空。删除先移除空目录，再删数据库行；数据库失败时重建目录。
- 四个命令都返回完整 `FolderInfo[]`，前端用返回值整体刷新树，不做本地路径推算。
- 文件夹计数由 `get_folders` 用一条直接计数查询在内存中沿路径前缀累加到祖先，结果随命令返回。
- `folders.sort_index` 已进 schema，但当前只有两个来源：新建行的默认值 0 与 v3 迁移时的名称顺序。手动排序命令（拖拽排序、`reorder_folders`）尚未实现，`FolderInfo` 也还没有 `sortIndex` 字段；P4 接入时补。

## 生命周期

- `create_library`：校验名称/绝对路径/目录状态 → 建目录 → 写清单 → 建库与 schema → 打开 → 把路径写入设置。
- `open_library_path`：校验清单/库 → 建连接池 → 校验或迁移 schema → 读计数 → 打开 → 把路径写入设置。空目录或无效目录直接拒绝，需先创建。
- 打开或创建新库成功后才替换旧会话；失败保留旧会话与设置中的旧路径。
- 启动：`open_configured_library` 按 `libraryPath` 自动打开；失败只记日志，前端随后再尝试一次并提示，最终退回欢迎页。
- `close_library`：释放连接、清空 AI 运行时状态与 `libraryPath`，不动文件。
- `rename_library`：更新清单名称与当前 `LibraryInfo.name`；不改目录名，也不写设置。
- 应用内没有多库列表与“移除图库”操作；换库由“打开另一个图库”完成。
- 导入/AI 任务/文件操作运行时，关闭、切换与创建图库都会被拒绝；需要先取消或等待任务结束。

## 已实现与未实现

已实现：上述单库生命周期、schema v3（含 v1→v2→v3 迁移）、连接池、设置 v3（含 v1→v2→v3 迁移）、窗口命令、Stage 0 UI；文件夹创建/重命名/移动/删除空文件夹与路径校验；打开图库时初始化追加式缩略图包（只扫描段文件）；`thumb://` 按图片 id 读取缩略图指针与 pack 字节；导入批次写入 `images`、`folders`、`tags`、`image_tags`、缩略图指针与 `thumb_segments` 行；S2 键集查询、基础筛选/排序、筛选计数、文件夹/标签列表、全部匹配 id、系统剪贴板文本，以及虚拟网格/列表；S3 搜索（描述/标签/文件夹，当前视图/全库）、时间/大小与存在性筛选、详情与批量编辑、标签/文件夹管理、集合 CRUD/排序/加入移出/并集交集差集、回收站软删除/恢复/永久删除/清空；S4 查看器源与 `img://` 原图/降采样预览协议、几何编辑保存（新哈希发布、缩略图/pHash/主色重算、embedding/pose 清除、旧文件清理）、复制图片到剪贴板、窗口全屏；S6 AI 进程托管、`/v1` 客户端、feature pack（embedding fp16 / pose f32）、均值中心化语义搜索、批量任务、查看器姿势生成。详见 [查看器与几何编辑](viewer-and-editor.md) 与 [AI 服务](ai-service.md)。

未实现（后续阶段）：拖出到资源管理器（OLE）、维护类长任务进度/取消（S8）、体验整改 P1–P4（见 [迁移后体验整改计划](../planning/2026-10-24-体验整改计划.md)）。对应命令在实现时追加到 `src-tauri/src/app/commands.rs` 的注册表，并同步本文件。

## 测试

`cargo test` 覆盖：

- 设置往返、v1 `lastLibraryId`/首项迁移、v2→v3 迁移（`thumbnailCacheMb` 被删除、新字段补默认值、旧值不丢）、非法主题与重复排序指标拒绝、设置页不覆盖后端 `libraryPath`、相对路径拒绝；
- schema 初始化、v1→v2 迁移、v2→v3 迁移（`folders.sort_index` 按名称编号）、大小写冲突拒绝、FTS 插入/更新、重复初始化拒绝；
- 目录规范校验（非法字符、保留设备名、结尾空格/句点、保留目录、根路径）；
- 图库创建/重开/重复创建拒绝/普通目录拒绝打开；
- 文件夹：多级创建补齐祖先、重命名/移动子树、仅大小写重命名、空文件夹删除与“仍有图片/子文件夹”拒绝、跨子树移动拒绝；
- 导入仓储插入、内容哈希冲突与合并元数据；
- 导入批次、取消、重复与 pack 缺失（见 [导入与缩略图](import.md)）；
- S2 查询：键集分页无重复、重复排序键的 id 决胜、文件夹/扩展名/评分/标签 any/all、升序/降序、回收站视图、筛选分面计数、文件夹/标签列表、全部匹配 id；文件夹子树筛选（后代命中、相似前缀不误匹配、根目录等价全库）、直接/子树计数、文件夹分面按子树展开；
- S3 查询：描述/标签名/文件夹路径搜索、全库范围包含回收站、时间/大小/存在性筛选、集合手工顺序、非法参数拒绝；
- S3 元数据：详情与主色、评分/描述更新、标签集合/增删/重命名/删除、文件夹移动与失败回滚、软删除/恢复/永久删除/清空回收站；
- S3 集合：CRUD/排序、加入去重、移出、并集/交集/差集；
- S4 查看器源：小图直接服务、预览缓存与复用、长边 4096 限制、EXIF 方向预览、原图缺失与回收站只读；
- S4 编辑保存：几何顺序与输出格式、旧文件清理、缩略图指针、元数据保留、feature_blobs 清除、冲突/无修改/回收站拒绝。

性能基准（默认忽略）见 [性能基准](../guides/performance.md)。

# 图库与元数据

图库是一个目录加上 `.artbank/` 数据区。应用侧由 `AppState` 持有唯一的当前图库会话，元数据由 SQLite 管理。本文件记录当前已实现的图库契约，包括单库设置 v6、schema v6、多级文件夹与操作记录/撤销；AI 任务与 feature pack 见 [AI 服务](ai-service.md)。

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
- schema 版本写在 `PRAGMA user_version` 与 `meta.schemaVersion`。当前为 6；版本高于程序支持值时拒绝打开。
- v1 → v2 迁移只新增 `folders(path COLLATE NOCASE)` 唯一索引。迁移前检查是否存在仅大小写不同的重复路径；有冲突则拒绝打开并保留 v1，不做自动改名或合并。
- v2 → v3 迁移给 `folders` 加 `sort_index INTEGER NOT NULL DEFAULT 0`，并按 `path`（NOCASE）顺序编号，升级后文件夹的显示顺序不变。
- v3 → v4 迁移只新增修改时间与像素尺寸两个索引，不改数据；20 万行建这两个索引合计 459 ms，39 万行的真实库首次打开按同量级估算在 1 s 左右。v1 库按 v1→v2→v3→v4 链式升级。
- v4 → v5 新增 `operations` / `operation_chunks` / `undo_runs` 三张表、`images.purge_operation_id` / `origin_operation_id` 两列与 `idx_images_purge` 索引，不改现有行；v1–v3 库按链式迁移到 v5。
- v5 → v6 给 `images` 增加 `optimized_at INTEGER NOT NULL DEFAULT 0`（0 表示没做过空间优化），不改现有行；v1–v5 库按链式迁移到 v6。

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
| `operations` / `operation_chunks` | 操作记录与逆数据；批量操作只记一条，大列表用原始 BLOB |
| `undo_runs` | 一次撤销或重做运行的起点、状态与起止时间 |
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
optimized_at       空间优化的写入时间；0 表示没压缩过
thumb_segment / thumb_offset / thumb_length / thumb_format
thumb_width / thumb_height / thumb_version
purge_operation_id 非空表示已从回收站移除、等待释放；文件与行暂留
origin_operation_id 导入批次；用于撤销导入时找回本次新增的行
```

- `deleted_at IS NULL` 表示正常；非空表示回收站。软删除不移动原图。
- 永久删除与清空回收站不再立即删行删文件，只写 `purge_operation_id`。这些行不出现在回收站、普通视图与全库搜索中；记录过期或清空操作记录后，`ReleaseGarbage` 任务才删行删原图。
- `operations.id` 使用 `AUTOINCREMENT`，记录清理后不复用；`purge_operation_id`、`origin_operation_id` 不建外键，记录行删除后标记仍可用于释放扫描。
- `thumb_segment = 0` 表示还没有缩略图；`thumb_version` 用于 URL 缓存失效。缩略图包追加写入，永久删除图片不会回收包内字节。
- `feature_blobs` 只存位置与模型/维度，不存向量本体；避免主库膨胀。
- FTS5 使用 `trigram` tokenizer，支持中文子串；查询少于 3 个字符时回退 `LIKE`（S3 实现）。
- 描述、标签、回收站、集合关系与 FTS 索引由 schema 触发器/外键维护；应用不手工写 FTS 表。
- 不保存原始文件名；原图文件名就是 `<content_hash>.<ext>`。列表显示尺寸、格式、大小、评分、标签与时间，不以文件名作为标识。

### 索引

- `images(deleted_at, score DESC, id DESC)`：评分首屏。
- `images(deleted_at, imported_at DESC, id DESC)`：导入时间排序。
- `images(deleted_at, modified_at DESC, id DESC)`：修改时间排序（schema v4）。
- `images(deleted_at, file_size DESC, id DESC)`：大小排序。
- `images(deleted_at, width * height DESC, id DESC)`：像素尺寸排序（表达式索引，schema v4）。
- `images(folder_id, deleted_at, imported_at DESC, id DESC)`：文件夹页。
- `folders(path COLLATE NOCASE)` UNIQUE：路径解析、重名检查与 v2 迁移。
- `images(deleted_at) WHERE deleted_at IS NOT NULL`：回收站。
- `images(purge_operation_id) WHERE purge_operation_id IS NOT NULL`：待释放批次（schema v5）。
- `image_tags(tag_id, image_id)`：标签反查。
- `collection_items(collection_id, position)`：集合顺序。

## 设置

设置文件固定为程序根目录的 `settings.json`，原子替换（临时文件 + rename）。程序根目录在 release 下是 exe 所在目录，debug 下是仓库根目录，两种模式都可用 `LILITH_ARTBANK_ROOT` 覆盖。日志在程序根目录的 `logs/`，不再写 AppData。

字段：

```text
settingsVersion    当前为 6
libraryPath        当前图库绝对路径；null 表示未打开。由后端维护
ai                 { baseUrl, port, autoStart, pythonPath, serverScript,
                     modelPath, clipModelPath }
performance        { readerConnections, workerThreads }
history            { maxSteps }  每个图库保留的操作记录步数，默认 100
mcp                { enabled, port, mode }  MCP 服务；默认关闭、18767、ask
ui                 { theme, gridThumbPx, thumbFit, thumbCacheCount,
                     sort, sortProfiles, viewerFilmstrip }
```

- 不使用旧 `%APPDATA%\com.lilith.artbank\settings.json`，也不做迁移；第一次启动在程序目录生成新文件。
- 程序目录不可写时启动失败并提示放到可写目录，不静默回退 AppData。
- `ai` 路径规则：
  - `pythonPath` 默认 `python_embeded\python.exe`；为空时只连接 `baseUrl` 上已启动的服务。
  - `serverScript` 默认 `server\main.py`；填了 Python 就必须填脚本。
  - `modelPath` 是模型根目录，默认 `resources\models`。
  - `clipModelPath` 是 CLIP 模型目录，默认 `siglip2-giant-opt-patch16-384`；相对 `modelPath`，也可填绝对路径。
  - 相对路径一律以程序根目录解析；本机 `baseUrl` 的端口必须与 `port` 一致。
- 设置页提交不会覆盖 `libraryPath`；它只由创建/打开/关闭图库更新。
- 保存启动相关 AI 设置后，应用会按新配置重启托管的 Python 服务。
- v1 的 `libraries[]` 与 `lastLibraryId` 在读取时迁移为单个 `libraryPath`：优先 `lastLibraryId` 对应条目，否则取第一个条目。迁移后不再写回旧字段。
- v2 → v3：删掉从未被任何代码读取的 `performance.thumbnailCacheMb`；新增的 `ui` 字段用默认值补齐（`thumbFit=cover`、`thumbCacheCount=1024`、`sort=[{key:"importedAt",descending:true}]`、`sortProfiles=[]`、`viewerFilmstrip=true`）；`theme` 增加 `system` 并成为默认值。`ai`、`performance`、`theme`、`gridThumbPx` 的旧值原样保留。
- v3 → v4：删除旧 `ai.executablePath`，换成 `pythonPath` / `serverScript` / `modelPath` / `clipModelPath`，缺失字段用默认值补齐；`baseUrl`、`port`、`autoStart` 保留。
- v4 → v5：新增 `history.maxSteps`（默认 100，范围 10–500）；保存后立即按新上限裁剪当前图库，被裁掉的永久删除记录会安排释放文件。
- v5 → v6：新增 `mcp`（默认 `enabled=false`、`port=18767`、`mode=ask`）；MCP 服务的实际能力见 [MCP 图库控制](mcp.md)。
- P0 曾加过 `ui.detailPanelMode`（`overlay`/`push`）；P1 确认详情面板只有“挤压画布 + 列数变化锚定”一种实现，字段已删除。旧设置文件里的 `detailPanelMode` 由 serde 忽略，不报错、不回写。
- 图库路径必须绝对；保存时去掉尾部路径分隔符，但保留盘符根（`G:\`）。
- `readerConnections` 1-16；`workerThreads` 为空表示自动。
- `history.maxSteps` 10-500；历史保存在各图库自己的 SQLite 中，上限是全局设置，换库后仍使用同一上限。
- `mcp.port` 1-65535；`mcp.mode` 只能是 `readOnly` / `ask` / `full`。批次 1 只有只读工具，实际档位固定为 `readOnly`。
- `theme` 只能是 `light` / `dark` / `system`；`gridThumbPx` 96-512；`thumbFit` 只能是 `cover` / `contain`；`thumbCacheCount` 256-8192。
- `sort` 至少一级、最多 7 级，指标只能取自 `importedAt` / `modifiedAt` / `score` / `size` / `pixels` / `folder` / `collectionPosition`，同一指标不能重复；`sortProfiles` 最多 50 个，名称非空且不区分大小写唯一，每个方案的 `sort` 同样校验。
- 生效范围：`theme` 由 `src/app/theme.ts` 解析并写到 `html[data-theme]`，`system` 跟随 `prefers-color-scheme` 实时切换；`thumbFit` 由浏览工具栏的“裁剪 / 适应”切换并即时写入；`thumbCacheCount` 由 `src/shared/thumbCache.ts` 作为缩略图 LRU 上限消费（改动后立即收缩）；`viewerFilmstrip` 由查看器状态栏的“胶片带”按钮消费；`sort` 是图库浏览的全局排序（集合内顺序离开集合时暂存，不写入设置），`sortProfiles` 是排序弹层里可保存的方案。
- 设置文件损坏时备份为 `settings.bad-<时间>.json` 并使用默认值，不阻塞启动。
- 主日志为 `logs/lilith-artbank.log`；AI 服务 stdout/stderr 为 `logs/ai-server.log`；stdio 小桥诊断为 `logs/mcp.log`。

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
| `query_images` | `{ query }` | `{ items, nextCursor, nextPageToken }` |
| `query_image_ids` | `{ query }` | `number[]` |
| `get_filter_counts` | `{ query }` | `FilterCounts` |
| `get_folders` | — | `FolderInfo[]` |
| `create_folder` | `{ path }` | `FolderInfo[]`（完整列表） |
| `rename_folder` | `{ folderId, name }` | `FolderInfo[]` |
| `move_folder` | `{ folderId, parentId }` | `FolderInfo[]` |
| `reorder_folders` | `{ folderIds, parentId, sortIndex }` | `FolderInfo[]` |
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
| `delete_images_permanently` | `{ imageIds }` | `DeleteReport`（`deferred=true`） |
| `empty_trash` | — | `DeleteReport`（`deferred=true`） |
| `get_operation_history` | — | `HistoryPage` |
| `undo_to_operation` | `{ targetId }` | `TaskStartResult`（撤销目标及上方尚未撤销的记录） |
| `redo_to_operation` | `{ targetId }` | `TaskStartResult`（从当前状态重做到目标记录） |
| `clear_operation_history` | — | `TaskStartResult`（清空记录并释放全部待删除文件） |
| `get_collections` | — | `CollectionInfo[]` |
| `create_collection` | `{ name }` | `CollectionInfo` |
| `rename_collection` | `{ collectionId, name }` | `null` |
| `delete_collection` | `{ collectionId }` | 删除条目数 |
| `reorder_collections` | `{ collectionIds }` | `null` |
| `add_images_to_collection` / `remove_images_from_collection` | `{ collectionId, imageIds }` | 变更条目数 |
| `combine_collections` | `{ mode, collectionIds, name }` | `CollectionInfo` |
| `create_collections_from_groups` | `{ groups, baseName }` | `CollectionInfo[]`（相似分组一次事务创建，只记一条历史） |
| `get_viewer_source` | `{ imageId }` | `ViewerSource` |
| `save_image_edit` | `{ request }` | `EditResult` |
| `estimate_edit_size` | `{ request }` | `EditSizeEstimate`（`fileSize`、`exact`） |
| `get_image_pose` | `{ imageId }` | `PoseData \| null`（feature pack） |
| `copy_image` | `{ imageId }` | `null` |
| `get_image_capabilities` | `{ imageIds }` | `ImageCapabilities[]`（pose / embedding / colors / phash） |
| `get_feature_summary` | — | `FeatureSummary`（全库描述 / embedding / 姿势 / pHash / 主色数量） |
| `start_batch_task` / `list_tasks` / `cancel_task` | 见 [AI 服务](ai-service.md) | 统一任务入口、任务列表与取消；事件 `task-progress` / `task-finished` |
| AI 命令（status/ensure/任务/描述/pose/语义搜索） | 见 [AI 服务](ai-service.md) | — |
| `window_set_fullscreen` | `{ value }` | `boolean`（写入后回读的真实状态） |
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

`FolderInfo`：`{ id, path, directImageCount, totalImageCount, sortIndex }`。`directImageCount` 只统计直接位于该文件夹的未删除图片，`totalImageCount` 包含全部后代；根目录的 `path` 为空字符串。`sortIndex` 只在同级内比较，决定“手动”排序模式下的顺序。

S3 的编辑/集合/回收站命令细节、返回结构与失败语义见 [浏览与检索](browse-and-query.md)。要点：

- 元数据写入都走单写连接；批量操作按 400 个 id 分块。
- 图片移动逐张 `rename` + 更新 `folder_id`，数据库失败时回滚文件；永久删除与清空回收站只标记 `purge_operation_id`，真正删行删文件由 `ReleaseGarbage` 任务按 400 张一块执行。
- 文件操作（图片移动、永久删除、清空回收站、撤销、释放）挂任务总线的 write lane，有进度与取消；导入、创建/打开图库要求任务总线空闲，关闭图库只拒绝 write lane（ai/scan 任务会被取消并等待退出）。单条记录的撤销与释放任务不可中途取消；多条记录的撤销只在记录之间检查取消，单条记录必须撤完。释放任务按队列等待。
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
- `folders.sort_index` 在新建、移动与手动排序时维护：新建与移动追加到同级末尾；`reorder_folders` 只接受同一父级的子文件夹，在事务里插入到指定位置并重新编号同级。前端“手动”排序模式按 `sortIndex` 显示，名称/图片数排序只是显示顺序，不会改写 `sortIndex`。

## 操作记录与撤销

历史保存在当前图库的 SQLite（`operations` / `operation_chunks` / `undo_runs`），换库后各自独立；最大步数来自设置 `history.maxSteps`。

- 已接入：移入回收站、恢复、永久删除、清空回收站（批次 1）；导入、编辑保存、图片移动、文件夹创建/重命名/移动/删除/排序（批次 2）；评分、描述（手工与单张 AI）、标签设置/增删/改名/删除、集合创建/重命名/删除/排序/加入/移出/运算、相似分组批量建集合、批量 AI 描述（批次 3）；图片空间优化（`kind='optimize'`，只记结果，不可撤销）。缩略图重建、embedding/pose 生成与相似扫描等派生数据不进入记录。
- 同步写入用 `history::write`：记录行、逆数据与业务更新在同一个 `IMMEDIATE` 事务里；`affected=0` 且没有逆数据时整条回滚，既不留下空记录，也不清空重做分支（没有实际改动的操作不算新时间线）。`write` 在同一个事务里删除 `undone` / `redoing` 记录：确实写入的新操作开启新时间线，灰色记录与其暂存文件不再可重做。
- 长任务用 `begin_live` 建 `pending` 记录、每批在同一个事务里用 `append_live_in_tx` / `add_live_affected_in_tx` 追加逆数据、结束时 `finish_live`；导入、批量 AI 描述与图片空间优化使用。`begin_live` 不结束重做分支：导入可能因 write lane 冲突根本没启动，或整批都是重复跳过；只有确实写入内容的批次（导入批次、AI 描述写入与压缩批次）才调用 `discard_redo_in_tx`。崩溃留下的 `pending` 记录在下次打开时按 `origin_operation_id` 或已有 chunk / `affected` 转为 `applied`，没有内容则删除。
- 图片空间优化不写任何逆数据：每批事务内更新 `images` 并累加 `affected`，完成后用 `history::update_label` 把「N 张 · 省 X」写进标题。`run_undo` 在标记 `undoing` 之前预检链中含 `optimize` 就返回“压缩操作不可撤销，原图已删除”，不会留下假的 `undoFailed`；记录面板同样禁用撤销。
- 撤销是线性回退：选中第 N 条 = 依次撤回当前仍为 `applied` 的记录，直到 N。成功的记录不再删除，而是标为 `undone`（列表里变灰）；`undone` 记录保持原 chunks，并通过 `redo` chunks 保存撤销前的状态。多条记录的撤销只在记录之间检查取消。
- 重做从当前状态出发，选中的目标是“重做到这里”：先找出目标及下方连续的 `undone` / `redoing` 记录，按 id 升序重新应用。重做前把整批记录标为 `redoing`；崩溃后这些标记就是续做范围，不会顺带重做用户没选中的更新记录。取消或失败时未完成的记录恢复为 `undone`；失败原因保存在 `error`，修正后可再次重做。
- 撤销失败会把记录标为 `undoFailed` 并保留错误；之后不能在它之前继续回退。重做失败或部分重做后，UI 会阻止继续向更早记录撤销，直到重试成功或清空记录。所有逆操作与重做操作都可重复执行。
- 裁剪只处理 `applied` / `undoFailed`，保留最新 `maxSteps` 条；每次成功写入、保存设置、打开图库与导入收尾后各裁剪一次。被裁掉的编辑/导入记录删除 `.artbank/undo/<id>/`，永久删除/清空记录安排 `ReleaseGarbage`。
- `ReleaseGarbage` 找 `purge_operation_id` 已没有对应 `operations` 行的批次，按 400 张一块删行、删文件；可重复执行，不可取消。清空操作记录会删掉全部记录与暂存目录，并安排释放全部待删除文件。
- 启动恢复在 `AppState::open_library` 做 DB 部分：pending 导入转 `applied` 或删除、收集最小 `undoing` id 与最大 `redoing` id、清理无主暂存目录与统计无主待释放批次；后台任务在 `application.manage` 之后安排，分别续做撤销与重做。
- `HistoryPage`：`{ entries, maxSteps, pendingPurgeCount, pendingPurgeBytes, canRun, blockedReason }`，`canRun` 表示此刻能否开始撤销或重做（没有任务占用 write lane 且列表非空）；`HistoryEntry`：`{ id, kind, label, createdAt, affected, status, error }`，`status` 为 `applied` / `undone` / `redoing` / `undoFailed`。`pendingPurge*` 统计全部待释放行，不区分记录是否仍可撤销。
- 待释放行对查询不可见：回收站视图与“全库搜索”显式加 `images.purge_operation_id IS NULL`；普通视图与集合视图靠 `deleted_at IS NULL` 挡住（待释放行一定有 `deleted_at`）；`rows_by_ids` 单独加同一条件。不要无条件加这条条件，它会让 20 万行按文件夹排序的首页从 39 ms 涨到 81 ms（见 [性能基准](../guides/performance.md)）。
- 界面入口在浏览工具栏“记录”按钮：灰色 `undone` 记录可选中重做，“当前状态”分隔线标出撤销位置；新操作后灰色记录消失。见 [浏览与检索](browse-and-query.md#操作记录)。

### 逆数据格式与撤销/重做语义

- 原始逆数据写在 `ids` / `triples` / `moves` / `json` chunks；撤销时另写 `redo` chunks（同一张表，`format='redo'`），只描述“撤销前是什么”，原始 chunks 不动。每次从 `applied` 进入撤销会先清掉旧 `redo` chunks；崩溃续做保留已写入的快照。
- 回收站/恢复：`triples(imageId, oldDeletedAt, oldModifiedAt)`。撤销写回旧状态；重做使用撤销时捕获的 `ImageStates`（当前 `deleted_at` / `modified_at`）。永久删除/清空回收站撤销时先记下 `purge_operation_id` 的图片 id，再清空标记；重做重新写标记。
- `moves`：`(imageId, fromFolderId, toFolderId, oldModifiedAt)`。撤销按当前 `folder_id` 判断：已是源文件夹就跳过，仍是目标文件夹才把文件移回并恢复 `modified_at`；文件移动失败会回滚本记录已完成的移动。重做按相反方向执行，并恢复撤销时的修改时间。移动时隐式新建的目标文件夹会在撤销后按空文件夹清理；重做按捕获的完整文件夹行重建。
- 文件夹：创建记录本次新建的全部 `(id, path)`（多级路径会同时新建祖先）；撤销时从深到浅，仅在仍为空时删目录与行，重做用原 id、路径、`created_at`、`sort_index` 补回。重命名/移动记录旧/新路径与旧/新 `sort_index`；撤销与重做都以当前路径判断方向。删除记录 `id`、路径、`created_at`、`sort_index`，撤销用原 id 重建行与目录，重做在仍为空时再次删除。排序记录同级全部旧 `sort_index`，撤销逐个恢复，重做使用撤销时捕获的目标排序位。
- 编辑：保存时在事务内暂存旧原图到 `.artbank/undo/<operationId>/`、写入编辑前完整内容快照（含 pHash / 主色 / `modified_at`）与本次新哈希。撤销要求当前哈希等于本次新哈希，用暂存文件恢复内容、重新编码缩略图、递增 `thumb_version`、清空 `feature_blobs`，并把新原图移入暂存目录（不删除）；重做使用撤销时捕获的完整内容状态，把新原图移回、恢复行与缩略图。撤销/重做都会删除另一侧的预览缓存；旧图暂存会保留以便再次撤销。
- 导入：新增行带 `origin_operation_id`；chunk 记录新建文件夹、新建标签与合并前快照。撤销按 origin 分块把原图移入 `.artbank/undo/<op>/`、捕获完整行（含标签、集合位置、缩略图指针）后删行，恢复合并项，再删除本次新建且仍为空的文件夹与无引用的标签。重做用原 id 插回行、移回原图、补标签/集合位置并恢复合并后的评分/时间，再重建本次新建的文件夹与标签。
- 评分：`triples(imageId, oldScore, oldModifiedAt)`。撤销恢复旧评分与修改时间，重做使用撤销时捕获的新值。
- 描述：JSON 记录旧描述、`description_source`、`description_model` 与 `modified_at`；手工描述与单张/批量 AI 描述共用同一套撤销。FTS 索引由现有触发器随 `description` 更新。
- 标签：`json` 记录旧关系、实际新增/删除的 `(imageId, tagId)` 与本次新建的标签。撤销恢复旧关系并删除已无引用的新建标签；删除标签的撤销用原 id 重建标签与关系，同名标签已存在时合并关系。重做按 `redo` chunks 重建被删标签、恢复关系或再次删除。
- 集合：创建/运算/相似分组记录完整快照（行 + `(imageId, position)`），撤销删除集合，重做按原 id 与条目重建；重命名、排序、加入、移出记录旧值，撤销恢复名称、`sort_order`、`updated_at` 与条目位置。相似分组一次事务创建全部集合，只产生一条记录。
- 编辑/导入暂存目录不会自动过期；记录裁剪、重做分支被新操作清空或“清空操作记录”时删除，启动时也会清理没有对应 `operations.id` 的目录。

## 生命周期

- `create_library`：校验名称/绝对路径/目录状态 → 建目录 → 写清单 → 建库与 schema → 打开 → 把路径写入设置。
- `open_library_path`：校验清单/库 → 建连接池 → 校验或迁移 schema → 读计数 → 打开 → 把路径写入设置。空目录或无效目录直接拒绝，需先创建。
- 打开或创建新库成功后才替换旧会话；失败保留旧会话与设置中的旧路径。
- 启动：`open_configured_library` 按 `libraryPath` 自动打开；失败只记日志，前端随后再尝试一次并提示，最终退回欢迎页。
- `close_library`：释放连接、清空 AI 运行时状态与 `libraryPath`，不动文件。
- `rename_library`：更新清单名称与当前 `LibraryInfo.name`；不改目录名，也不写设置。
- 应用内没有多库列表与“移除图库”操作；换库由“打开另一个图库”完成。
- 导入/AI 任务/文件操作运行时，创建与打开图库会被拒绝；关闭图库在 write 任务进行时拒绝，只有 ai/scan 任务时先取消再关闭。

## 已实现与未实现

已实现：上述单库生命周期、schema v6（含 v1→v2→v3→v4→v5→v6 迁移）、连接池、设置 v6（含 v1→v2→v3→v4→v5→v6 迁移；文件移到程序目录）、窗口命令、Stage 0 UI；文件夹创建/重命名/移动/删除空文件夹、路径校验与手动排序（`reorder_folders` / `sort_index`）；打开图库时初始化追加式缩略图包（只扫描段文件）；`thumb://` 按图片 id 读取缩略图指针与 pack 字节；导入批次写入 `images`、`folders`、`tags`、`image_tags`、缩略图指针与 `thumb_segments` 行；S2 键集查询、基础筛选/排序、筛选计数、文件夹/标签列表、全部匹配 id、系统剪贴板文本，以及虚拟网格/列表；S3 搜索（描述/标签/文件夹，当前视图/全库）、时间/大小与存在性筛选、详情与批量编辑、标签/文件夹管理、集合 CRUD/排序/加入移出/并集交集差集、回收站软删除/恢复/永久删除/清空；S4 查看器源与 `img://` 原图/降采样预览协议、几何编辑保存（新哈希发布、缩略图/pHash/主色重算、embedding/pose 清除、旧文件清理）、复制图片到剪贴板；S6 AI 进程托管、`/v1` 客户端、feature pack（embedding fp16 / pose f32）、均值中心化语义搜索、批量任务、查看器姿势生成；P2 任务总线与任务中心、缩略图重建、批量文件操作进度与取消、`get_image_capabilities` / `get_feature_summary`；P3 编辑体积预估（`estimate_edit_size`）、保存后行内就地更新、标签点击筛选与锚点跳转、`window_set_fullscreen` 设置语义；P4 多级排序与 id 列表游标、`anchor_id`、`nextPageToken`、排序方案设置、文件夹与排序索引；操作记录批次 1–3（schema/设置 v5、回收站与删除撤销、延迟释放、历史面板；导入/编辑/移动/文件夹撤销与中断续做；灰色重做分支、当前状态分隔线与撤销/重做续做；评分/描述/AI 描述/标签/集合/相似分组撤销与重做）。详见 [查看器与几何编辑](viewer-and-editor.md) 与 [AI 服务](ai-service.md)。

未实现（后续阶段）：拖出到资源管理器（OLE）、维护类工作流（描述导出/回填、孤儿清理、包压缩、库检查、导出）。对应命令在实现时追加到 `src-tauri/src/app/commands.rs` 的注册表，并同步本文件。

## 测试

`cargo test` 覆盖：

- 设置往返、v1 `lastLibraryId`/首项迁移、v2→v3 迁移（`thumbnailCacheMb` 被删除、新字段补默认值、旧值不丢）、v4→v5 迁移（补 `history.maxSteps=100`）、操作记录步数范围拒绝、非法主题与重复排序指标拒绝、设置页不覆盖后端 `libraryPath`、相对路径拒绝；
- schema 初始化、v1→v2 迁移、v2→v3 迁移（`folders.sort_index` 按名称编号）、v3→v4 迁移（新增修改时间与像素索引）、v4→v5 迁移（新增操作表、待释放列与索引且保留现有行）、v5→v6 迁移（新增 `optimized_at` 且旧行为 0）、大小写冲突拒绝、FTS 插入/更新、重复初始化拒绝；
- 目录规范校验（非法字符、保留设备名、结尾空格/句点、保留目录、根路径）；
- 图库创建/重开/重复创建拒绝/普通目录拒绝打开；
- 文件夹：多级创建补齐祖先、重命名/移动子树、仅大小写重命名、空文件夹删除与“仍有图片/子文件夹”拒绝、跨子树移动拒绝；
- 导入仓储插入、内容哈希冲突与合并元数据；
- 导入批次、取消、重复与 pack 缺失（见 [导入与缩略图](import.md)）；
- S2 查询：键集分页无重复、重复排序键的 id 决胜、文件夹/扩展名/评分/标签 any/all、升序/降序、回收站视图、筛选分面计数、文件夹/标签列表、全部匹配 id；文件夹子树筛选（后代命中、相似前缀不误匹配、根目录等价全库）、直接/子树计数、文件夹分面按子树展开；
- P4 查询：6 级排序跨页无重复无遗漏（重复值 + 混合方向 + 两个文件夹）、文件夹排序走 id 游标、锚点包含目标行、排序级数/重复指标/游标长度校验；文件夹同级重排、跨父级拒绝；
- S3 查询：描述/标签名/文件夹路径搜索、全库范围包含回收站、时间/大小/存在性筛选、集合手工顺序、非法参数拒绝；
- S3 元数据：详情与主色、评分/描述更新、标签集合/增删/重命名/删除、文件夹移动与失败回滚、软删除/恢复、永久删除与清空回收站的延迟标记与释放；
- 操作记录：空记录回滚、线性撤销回收站/恢复、永久删除标记撤销、待释放批次释放、裁剪保留最新、取消在记录之间、`pending`/`undoing` 启动恢复、无主暂存目录清理、重开图库后历史仍可读；批次 2 覆盖移动撤销（文件与行、部分恢复后续做）、编辑撤销（文件、行、缩略图、`feature_blobs`、暂存目录）、文件夹创建/重命名/移动/删除/排序撤销，以及导入新增/合并撤销（新建文件夹与标签清理、合并快照恢复）；重做覆盖回收站/恢复、永久删除标记、移动、编辑、文件夹创建/重命名/移动/排序/删除链条与导入新增/合并，另测“重做到较旧记录时保留更新记录为灰”、新操作清空重做分支、写入内容的导入清空重做分支而空 `pending` 导入保留、`redoing` 启动续做；批次 3 覆盖评分/描述/AI 描述、标签设置/增删/改名/删除、集合 CRUD/排序/加入移出/运算/相似分组的线性撤销与重做。
- 图片空间优化：判据与阶梯编码、PSNR/坏点、EXIF/ICC 搬运、估算分层与只读、凭证指纹与候选 ±10%、执行后内容列与 `optimized_at` 更新而其余元数据不变、旧图删除、重复压缩跳过、`pending` 记录恢复为 `applied`。
- S3 集合：CRUD/排序、加入去重、移出、并集/交集/差集；
- S4 查看器源：小图直接服务、预览缓存与复用、长边 4096 限制、EXIF 方向预览、原图缺失与回收站只读；
- S4 编辑保存：几何顺序与输出格式、旧文件清理、缩略图指针、元数据保留、feature_blobs 清除、冲突/无修改/回收站拒绝。

性能基准（默认忽略）见 [性能基准](../guides/performance.md)。

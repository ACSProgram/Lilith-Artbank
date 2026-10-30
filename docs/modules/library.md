# 图库与元数据

图库是一个目录加上 `.artbank/` 数据区。应用侧由 `AppState` 持有唯一的当前图库会话，元数据由 SQLite 管理。本文件记录当前已实现的图库契约，包括单库设置 v7、schema v10、多级文件夹、文件夹与图片的回收站、临时文件夹与操作记录/撤销；AI 任务与 feature pack 见 [AI 服务](ai-service.md)。

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
- schema 版本写在 `PRAGMA user_version` 与 `meta.schemaVersion`。当前为 10；版本高于程序支持值时拒绝打开。
- v1 → v2 迁移只新增 `folders(path COLLATE NOCASE)` 唯一索引。迁移前检查是否存在仅大小写不同的重复路径；有冲突则拒绝打开并保留 v1，不做自动改名或合并。
- v2 → v3 迁移给 `folders` 加 `sort_index INTEGER NOT NULL DEFAULT 0`，并按 `path`（NOCASE）顺序编号，升级后文件夹的显示顺序不变。
- v3 → v4 迁移只新增修改时间与像素尺寸两个索引，不改数据；建索引耗时与首次打开停顿见[性能基准](../guides/performance.md)。v1 库按 v1→v2→v3→v4 链式升级。
- v4 → v5 新增 `operations` / `operation_chunks` / `undo_runs` 三张表、`images.purge_operation_id` / `origin_operation_id` 两列与 `idx_images_purge` 索引，不改现有行；v1–v3 库按链式迁移到 v5。
- v5 → v6 给 `images` 增加 `optimized_at INTEGER NOT NULL DEFAULT 0`（0 表示没做过空间优化），不改现有行。
- v6 → v7 给 `folders` 增加 `kind TEXT NOT NULL DEFAULT 'normal' CHECK (kind IN ('normal','temp'))` 与唯一部分索引 `idx_folders_single_temp`，并插入唯一的临时文件夹行；内部路径固定为 `.artbank-temp`，已有同名文件夹时追加 `-1`、`-2` 后缀。v1–v6 库按链式迁移到 v7。
- v7 → v8 只新增覆盖索引 `idx_images_visible_facets(deleted_at, purge_operation_id, folder_id, file_ext, score)`，不改数据与语义；收益与建索引成本见[性能基准](../guides/performance.md)。
- v8 → v9 只新增覆盖索引 `idx_images_visible_bytes(deleted_at, folder_id, file_size)`，供文件夹概览的 `SUM(file_size)` 在索引内完成，不改数据与语义。分面索引保持窄，避免标签聚合因索引变宽而退化。
- v9 → v10 给 `folders` 增加 `trashed_at` 与 `purge_operation_id` 两列，并建两个空集上的部分索引；只做 `ALTER TABLE ADD COLUMN`，不改现有行。旧 `folderDelete`（空文件夹物理删除）记录仍可解析与撤销。

### 表

| 表 | 作用 |
| --- | --- |
| `meta` | `appFormat`、`schemaVersion`、`libraryId`、`createdAt` |
| `folders` | 图库内文件夹；`path` 为 `/` 分隔相对路径，根目录为 `''`；每个祖先都有行，路径 NOCASE 唯一；`sort_index` 为手动排序位；`kind` 为 `normal` / `temp`，临时文件夹每个图库恰好一个；`trashed_at` 非空表示整棵子树在回收站，`purge_operation_id` 非空表示等待释放 |
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
- 永久删除与清空回收站不再立即删行删文件，只写 `purge_operation_id`。这些图片与文件夹行不出现在回收站、回收站树、普通视图与全库搜索中；记录过期后自动安排 `ReleaseGarbage` 删除；清空操作记录本身不删文件，之后用「释放待删文件」删除。
- `operations.id` 使用 `AUTOINCREMENT`，记录清理后不复用；`purge_operation_id`、`origin_operation_id` 不建外键，记录行删除后标记仍可用于释放扫描。
- `thumb_segment = 0` 表示还没有缩略图；`thumb_version` 用于 URL 缓存失效。缩略图包追加写入，永久删除图片不会回收包内字节。
- 缩略图段在追加 8 MiB、换段、指针事务提交前与切换/关闭图库时 `sync_all`，其余追加只 `flush`；打开图库时按 `thumb_segments.size` 与磁盘实际长度比对，缺段或截断时把越界指针清成 `thumb_segment = 0`，这些行重新算「缺缩略图」并由重建任务补上。
- `feature_blobs` 只存位置与模型/维度，不存向量本体；避免主库膨胀。
- FTS5 使用 `trigram` tokenizer，支持中文子串；查询少于 3 个字符时回退 `LIKE`。
- 描述、标签、回收站、集合关系与 FTS 索引由 schema 触发器/外键维护；应用不手工写 FTS 表。
- 不保存原始文件名；原图文件名就是 `<content_hash>.<ext>`。列表显示尺寸、格式、大小、评分、标签与时间，不以文件名作为标识。

### 索引

- `images(deleted_at, score DESC, id DESC)`：评分首屏。
- `images(deleted_at, imported_at DESC, id DESC)`：导入时间排序。
- `images(deleted_at, modified_at DESC, id DESC)`：修改时间排序（schema v4）。
- `images(deleted_at, file_size DESC, id DESC)`：大小排序。
- `images(deleted_at, width * height DESC, id DESC)`：像素尺寸排序（表达式索引，schema v4）。
- `images(folder_id, deleted_at, imported_at DESC, id DESC)`：文件夹页。
- `images(deleted_at, purge_operation_id, folder_id, file_ext, score)`：可见集分面与计数覆盖索引（schema v8）。
- `images(deleted_at, folder_id, file_size)`：文件夹概览的字节汇总覆盖索引（schema v9）。
- `folders(path COLLATE NOCASE)` UNIQUE：路径解析、重名检查与 v2 迁移。
- `folders(trashed_at) WHERE trashed_at IS NOT NULL`：回收站文件夹（schema v10）。
- `folders(purge_operation_id) WHERE purge_operation_id IS NOT NULL`：待释放文件夹（schema v10）。
- `images(deleted_at) WHERE deleted_at IS NOT NULL`：回收站。
- `images(purge_operation_id) WHERE purge_operation_id IS NOT NULL`：待释放批次（schema v5）。
- `image_tags(tag_id, image_id)`：标签反查。
- `collection_items(collection_id, position)`：集合顺序。

## 设置

设置文件固定为程序根目录的 `settings.json`，原子替换（临时文件 + rename）。程序根目录在 release 下是 exe 所在目录，debug 下是仓库根目录，两种模式都可用 `LILITH_ARTBANK_ROOT` 覆盖。日志在程序根目录的 `logs/`，不再写 AppData。

字段：

```text
settingsVersion    当前为 7
libraryPath        当前图库绝对路径；null 表示未打开。由后端维护
ai                 { baseUrl, port, autoStart, pythonPath, serverScript,
                     modelPath, clipModelPath, prewarmImports }
performance        { readerConnections, workerThreads }
history            { maxSteps }  每个图库保留的操作记录步数，默认 100
maintenance        { autoCompactThumbs }  空闲时自动回收缩略图段；默认 true，缺键按默认值
import             { optimize }  「导入时压缩」选项；null 表示关闭（默认）
mcp                { enabled, port, mode }  MCP 服务；默认关闭、18767、ask
ui                 { theme, gridThumbPx, thumbFit, thumbCacheCount,
                     sort, sortProfiles, viewerFilmstrip, folderSplit,
                     folderSplitRatio, listColumns, similarityKeepSort }
```

- 不使用旧 `%APPDATA%\com.lilith.artbank\settings.json`，也不做迁移；第一次启动在程序目录生成新文件。
- 程序目录不可写时启动失败并提示放到可写目录，不静默回退 AppData。
- `ai` 路径规则：
  - `pythonPath` 默认 `python_embeded\python.exe`；为空时只连接 `baseUrl` 上已启动的服务。
  - `serverScript` 默认 `server\main.py`；填了 Python 就必须填脚本。
  - `modelPath` 是模型根目录，默认 `resources\models`。
  - `clipModelPath` 是 CLIP 模型目录，默认 `siglip2-giant-opt-patch16-384`；相对 `modelPath`，也可填绝对路径。
  - `prewarmImports` 默认 true：启动托管的服务时加 `--prewarm-imports`，让服务在后台导入 torch/transformers。代价是服务进程常驻约 420 MB，收益是首次 AI 请求少等 7–8 s。
  - 相对路径一律以程序根目录解析；本机 `baseUrl` 的端口必须与 `port` 一致。
- 设置页提交不会覆盖 `libraryPath`；它只由创建/打开/关闭图库更新。同理，`import` 组由导入对话框的窄命令 `set_import_optimize` 维护，设置页提交整份 `Settings` 时后端保留原值，改主题不会把导入压缩开关抹掉。
- `import.optimize` 只记「导入时压缩」的开关与参数（`{ tier, outputFormat }`，`null` 为关闭）；没有这个键时按默认关闭读取，不需要升版本号。细节见[导入](import.md#导入时压缩默认关闭选项持久化)。
- 保存启动相关 AI 设置后，应用会按新配置重启托管的 Python 服务。
- v1 的 `libraries[]` 与 `lastLibraryId` 在读取时迁移为单个 `libraryPath`：优先 `lastLibraryId` 对应条目，否则取第一个条目。迁移后不再写回旧字段。
- v2 → v3：删掉从未被任何代码读取的 `performance.thumbnailCacheMb`；新增的 `ui` 字段用默认值补齐（`thumbFit=cover`、`thumbCacheCount=1024`、`sort=[{key:"importedAt",descending:true}]`、`sortProfiles=[]`、`viewerFilmstrip=true`）；`theme` 增加 `system` 并成为默认值。`ai`、`performance`、`theme`、`gridThumbPx` 的旧值原样保留。
- v3 → v4：删除旧 `ai.executablePath`，换成 `pythonPath` / `serverScript` / `modelPath` / `clipModelPath`，缺失字段用默认值补齐；`baseUrl`、`port`、`autoStart` 保留。
- v4 → v5：新增 `history.maxSteps`（默认 100，范围 10–500）；保存后立即按新上限裁剪当前图库，被裁掉的永久删除记录会安排释放文件。
- v5 → v6：新增 `mcp`（默认 `enabled=false`、`port=18767`、`mode=ask`）；MCP 服务的实际能力见 [MCP 图库控制](mcp.md)。
- `ui.detailPanelMode` 已删除：详情面板只有“挤压画布 + 列数变化锚定”一种实现。旧设置文件里的该字段由 serde 忽略，不报错、不回写。
- 图库路径必须绝对；保存时去掉尾部路径分隔符，但保留盘符根（`G:\`）。
- `readerConnections` 1-16；`workerThreads` 1-128，为空表示自动（逻辑核数 − 1）。它是全局后台图像工作预算，导入、压缩、估算、缩略图、查看器预览、AI 解码与 MCP 拼图共用；保存后对之后开始的工作立即生效。
- `history.maxSteps` 10-500；历史保存在各图库自己的 SQLite 中，上限是全局设置，换库后仍使用同一上限。
- `mcp.port` 1-65535；`mcp.mode` 只能是 `readOnly` / `ask` / `full`，工具按当前档位实时判定。
- `theme` 只能是 `light` / `dark` / `system`；`gridThumbPx` 96-512；`thumbFit` 只能是 `cover` / `contain`；`thumbCacheCount` 256-8192；`folderSplit` 默认 `false`，`folderSplitRatio` 0.2-0.8（默认 0.5）。这两个字段缺失时按默认值补齐，不升版本号。
- `sort` 至少一级、最多 7 级，指标只能取自 `importedAt` / `modifiedAt` / `score` / `size` / `pixels` / `folder` / `collectionPosition`，同一指标不能重复；`sortProfiles` 最多 50 个，名称非空且不区分大小写唯一，每个方案的 `sort` 同样校验。
- `ui.listColumns` 只能从 `pixels` / `format` / `size` / `score` / `tags` / `importedAt` / `folder` 里选（缩略图列固定显示，不在其中），至少要保留一列，认不出的列按不显示处理。
- `ui.similarityKeepSort` 是相似组合并的保留规则，默认 `[评分降序, 导入时间降序]`；可以留空（读取时回落默认值），但不能用 `collectionPosition`，其余校验同 `sort`。
- 生效范围：`theme` 由 `src/app/theme.ts` 解析并写到 `html[data-theme]`，`system` 跟随 `prefers-color-scheme` 实时切换；`thumbFit` 由浏览工具栏的“裁剪 / 适应”切换并即时写入；`thumbCacheCount` 由 `src/shared/thumbCache.ts` 作为缩略图 LRU 上限消费（改动后立即收缩）；`viewerFilmstrip` 由查看器状态栏的“胶片带”按钮消费；`folderSplit` / `folderSplitRatio` 由侧栏文件夹树标题栏的开关与分隔条消费，拖动结束或按键调整后写入；`sort` 是图库浏览的全局排序（集合内顺序离开集合时暂存，不写入设置），`sortProfiles` 是排序弹层里可保存的方案；`similarityKeepSort` 由相似分组对话框的“保留规则”编辑并即时写入。
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
| `restart_app` | — | `null`（请求重启进程；退出流程仍走 `RunEvent::Exit`） |
| `get_library_status` | — | `LibraryInfo \| null`（只返回缓存计数，不重算） |
| `get_library_counts` | — | `{ imageCount, trashCount, tempCount }`（重算并写回缓存） |
| `rename_library` | `{ name }` | `LibraryInfo`（只改名称，不改目录） |
| `query_images` | `{ query }` | `{ items, nextCursor, nextPageToken }` |
| `query_image_ids` | `{ query }` | `number[]` |
| `get_library_stats` | `{ request: StatsRequest }` | `StatsSnapshot`（统计契约见[浏览与检索](browse-and-query.md)） |
| `get_folders` | — | `FolderInfo[]` |
| `create_folder` | `{ path }` | `FolderChange`（新建的行，含自动补齐的缺失祖先） |
| `rename_folder` | `{ folderId, name }` | `FolderChange`（整个子树，路径都变了） |
| `move_folder` | `{ folderId, parentId }` | `FolderChange`（子树 + 两侧祖先） |
| `place_folders` | `{ folderIds, parentId, sortIndex? }` | `FolderChange`（多文件夹一次移动/插入，一条记录） |
| `reorder_folders` | `{ folderIds, parentId, sortIndex }` | `FolderChange`（同级受影响行） |
| `apply_folder_order` | `{ folderId, key, descending, recursive, orders }` | `FolderChange`（一次性排序，一条记录） |
| `delete_folder` | `{ folderId }` | `FolderChange`（整棵子树移入回收站；`removedIds` 含子树 id，`affectedImages` 为一起进入回收站的图片数） |
| `get_trash_folders` | — | `TrashFolders`（回收站文件夹、根行直属图片数、回收站总图片数） |
| `restore_folder` | `{ folderId }` | `FolderChange`（整个删除单元回到正常树） |
| `delete_folder_permanently` | `{ folderId }` | `FolderChange`（只写文件夹与图片的待释放标记） |
| `get_tags` | `{ limit? }` | `TagInfo[]` |
| `search_tags` | `{ query, limit? }` | `TagInfo[]` |
| `get_image_detail` | `{ imageId }` | `ImageDetails` |
| `get_image_states` | `{ imageIds }` | `ImageState[]`（最多 1000 个 id；不存在的 id 缺席） |
| `update_image_score` | `{ imageId, score }` | `null` |
| `set_images_score` | `{ imageIds, score }` | 更新行数 |
| `update_image_description` | `{ imageId, description }` | `null` |
| `set_image_tags` | `{ imageId, tagNames }` | `TagRef[]` |
| `add_tags_to_images` | `{ imageIds, tagNames }` | 新增关系数 |
| `remove_tags_from_images` | `{ imageIds, tagNames }` | 删除关系数 |
| `rename_tag` | `{ tagId, name }` | `null` |
| `delete_tag` | `{ tagId }` | 删除关系数 |
| `merge_tags` | `{ sourceId, targetId }` | `MergeReport`（`sourceImages` / `moved` / `duplicate` / 两个名称） |
| `move_images_to_folder` | `{ imageIds, folderPath }` | `MoveReport` |
| `trash_images` / `restore_images` | `{ imageIds }` | 更新行数 |
| `delete_images_permanently` | `{ imageIds }` | `DeleteReport`（`deferred=true`） |
| `empty_trash` | — | `DeleteReport`（`deferred=true`） |
| `get_operation_history` | — | `HistoryPage` |
| `undo_to_operation` | `{ targetId }` | `TaskStartResult`（撤销目标及上方尚未撤销的记录） |
| `redo_to_operation` | `{ targetId }` | `TaskStartResult`（从当前状态重做到目标记录） |
| `retry_undo_operation` | `{ targetId }` | `TaskStartResult`（只重试这条撤销失败的记录） |
| `skip_undo_operation` | `{ targetId }` | `null`（标为 `undoSkipped`，不再参与撤销与重做） |
| `release_pending_files` | — | `TaskStartResult`（删除没有记录引用的待释放文件） |
| `clear_operation_history` | — | `TaskStartResult`（只清空记录与撤销数据，不删原图） |
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
| `get_feature_summary` | — | `FeatureSummary`（普通图库的描述 / embedding / 姿势 / pHash / 主色数量；不含临时区） |
| `start_batch_task` / `list_tasks` / `cancel_task` | 见 [AI 服务](ai-service.md) | 统一任务入口、任务列表与取消；事件 `task-progress` / `task-finished` |
| AI 命令（status/ensure/任务/描述/pose/语义搜索） | 见 [AI 服务](ai-service.md) | — |
| `window_set_fullscreen` | `{ value }` | `boolean`（写入后回读的真实状态） |
| `copy_text` | `{ text }` | `null` |
| `window_minimize` / `window_toggle_maximize` / `window_is_maximized` / `window_close` | — | 窗口控制 |
| `get_app_info` | — | `{ version, settingsPath, logPath }` |
| `reveal_path` | `{ path }` | `null`（在系统文件管理器里选中该文件，或打开该目录） |

主窗口的位置、大小与最大化由 `tauri-plugin-window-state` 持久化到系统应用配置目录，不保存全屏。启动后 `app::window_state::ensure_main_window_visible` 检查窗口与任一显示器是否至少有 120×40 物理像素的交集，完全离屏时移回主显示器并收缩到可见范围。

`LibraryInspection`：

```text
{ kind: "library", manifest }
{ kind: "empty" }
{ kind: "invalid", reason }
```

`LibraryInfo`：`id`、`name`、`path`、`imageCount`、`trashCount`、`tempCount`、`schemaVersion`、`openedAt`。计数分别对应普通图片、回收站与临时文件夹；临时图片不计入 `imageCount`。打开图库不再同步算计数（真实库 620 ms），`imageCount` 等字段先为 0，前端随后调用 `get_library_counts` 补齐；`get_library_status` 只返回缓存值，重算只发生在开库与数据变化之后。

`FolderChange`：`{ folders: FolderInfo[], removedIds: number[], affectedImages: number }`。文件夹写命令只回传受影响的行，前端按 id 就地合并（`applyFolderChange`），不再每次重取整棵树；`affectedImages` 只用于文件夹移入/移出回收站，其它写命令为 0。

`FolderInfo`：`{ id, path, directImageCount, totalImageCount, sortIndex }`。`directImageCount` 只统计直接位于该文件夹的未删除图片，`totalImageCount` 包含全部后代；根目录的 `path` 为空字符串。`sortIndex` 只在同级内比较，决定文件夹树的显示顺序。

编辑/集合/回收站命令的细节、返回结构与失败语义见 [浏览与检索](browse-and-query.md)。要点：

- 元数据写入都走单写连接；批量操作按 400 个 id 分块。
- 图片移动逐张 `rename` + 更新 `folder_id`，数据库失败时回滚文件；永久删除与清空回收站只标记 `purge_operation_id`（回收站图片与文件夹都可能被标记），真正删行删文件由 `ReleaseGarbage` 任务按 400 张一块执行。
- 任务准入由资源表决定（`src-tauri/src/concurrency/resources.rs`），不再是一条全局 write lane：导入占内容写者 + 时间线读 + 目标文件夹子树读，压缩占内容写者 + 根路径原地替换（`Access::Replace`），估算只占会话读并逐张拿文件夹读锁，缩略图重建占内容写者（避免用旧原图算出的缩略图覆盖新内容），移动/编辑保存/释放占内容写者与对应文件夹路径，永久删除/清空回收站占元数据写 + 时间线读，文件夹操作占对应子树路径（新建与移动还锁父级，避免同级序号并发），AI 任务占 `ai.service` 与内容写者，并在读原图时按文件夹加短读锁。冲突时长任务除撤销/重做/清空记录外自动排队，文件夹维护最多等当前一次读图 10 秒、超时报冲突；同一资源先来先得，路径锁写者优先。短写（评分、标签、描述、集合成员、回收站）只占元数据写，冲突直接失败；排队中的长任务不占资源，因此不会让这些短写失败——等待者只在路径锁上保留写者优先。
- 创建/打开图库要求资源表完全空闲（含等待者）；关闭图库拒绝未完成的写任务（导入、移动、永久删除、清空回收站、压缩、清理孤儿、撤销/重做），AI/估算/缩略图/相似分组/检查/导出/包回收/释放任务会被取消并等待退出。关库先停空闲回收线程（join，之后不可能再提交自动回收），再取消任务，最后取会话锁；任务总线带图库代次，排队者在拿到资源时若发现代次已变就直接落「已取消」，不会拿着旧句柄写旧库。单条记录的撤销不可中途取消；多条记录的撤销与释放任务只在块边界检查取消——释放停下时，未处理的行仍带待释放标记，下次释放接着删。任务线程 panic 由框架捕获、写日志并标为失败，资源随 guard 释放；正常返回但没有写终态同样标为失败。运行中/排队任务不会被 50 条任务历史裁剪。
- 集合只引用图片 id；集合运算新建集合，不修改源集合。
- 解码内存另有一份全进程字节额度（1.5 GiB，不随设置变化）：导入、空间优化、估算样本编码、查看器预览、AI 输入解码与 MCP 拼图都按各自的解码口径估算后申请；任务路径的等待随任务取消，查看器与 MCP 拼图没有取消入口，单张估算超过上限时独占，等当前占用归零后放行。申请顺序统一为线程许可在前、字节额度在后。

## 文件夹

文件夹路径既是显示层级也是磁盘位置：`folders.path` 的每一段对应图库根目录下的同名目录。

- 路径规范：`/` 分隔、无空段、不能以 `/` 开头或结尾；每段拒绝 Windows 非法字符 `<>:"\|?*`、控制字符、结尾空格/句点、保留设备名（`CON`、`PRN`、`AUX`、`NUL`、`COM1-9`、`LPT1-9` 等）。
- 任何层级都拒绝 `.artbank`、`recycle`、`output`；根目录不能创建、重命名、移动或删除。
- 新建可以给出多级路径；不存在的祖先会同时创建物理目录与数据库行。路径按 NOCASE 判重，大小写不同也视为已存在。
- 新建、移动与导入补齐祖先时，新行都拼在**库里那一行**的路径后面（`ensure_folder_path` 返回规范路径），不沿用调用方的大小写：库里是 `Art`、请求是 `art/sub` 时，数据库行一律是 `Art/sub`；新建与移动的物理目录也按规范路径创建，导入写文件仍用工作线程传入的文件夹路径（Windows 下指向同一目录）。判断祖先/子树关系一律忽略 ASCII 大小写（`paths::folder_within`），与 `COLLATE NOCASE` 的唯一索引同口径；“移动到自身或后代”的守卫、同级兄弟比较与子树计数都按这个口径，避免旧数据里前缀大小写不同导致守卫失效或计数丢失。
- 重命名只改最后一段，保留父路径；仅大小写变化时通过临时目录中转（Windows 不能直接改大小写）。移动接收目标父文件夹 id，拒绝移动到自身或后代。
- 重命名/移动会先改物理目录，再在单写事务里更新整个子树的 `path`；数据库失败时把目录移回。目标位置已有同名文件夹或磁盘目录时拒绝。
- `place_folders` 一次把多个文件夹移动到同一父级，可插入到指定同级位置；父子同时选中只保留最外层父。先校验全部目录与名称冲突，再依次 rename，任一失败反向搬回；整次操作一条 `FolderMove` 记录。
- 删除文件夹 = 整棵子树移入回收站：文件夹与全部子文件夹写同一个 `trashed_at`，其中仍正常的图片写同一个 `deleted_at`，物理文件与目录都留在原路径。空文件夹也走同一语义，不再有“只能删除空文件夹”的特例。正常树、导入目标、任务范围与 MCP 文件夹列表都看不到回收站文件夹；新建、重命名、移动、导入到已删除路径会被拒绝并提示先恢复或永久删除。
- 恢复文件夹 = 恢复整个删除单元：选中删除单元里的任意一级，向上找到时间戳相同的最外层文件夹，整棵子树与随这次删除一起进入回收站的图片一起恢复。删除前已单独在回收站的图片保持删除；上级文件夹来自更早的另一次删除时拒绝恢复，先恢复上级文件夹。单张恢复图片前会在同一个写事务里确认所在文件夹不在回收站，因此不会与整目录删除擦肩，留下「正常图片位于已删除文件夹」的行。
- 永久删除回收站文件夹只给子树文件夹与其中回收站图片写 `purge_operation_id`；文件夹立即从回收站树消失，行与空目录等释放任务清理。目录里仍有未纳管文件时保留行与待释放标记、不递归强删，并把这条写进释放任务的失败清单与日志；它不算「还有待释放文件」，等下次清空回收站重新标记时再试。
- 文件夹命令都返回 `FolderChange`：`folders` 是受影响文件夹的最新状态，`removedIds` 是从正常树移除的子树 id，`affectedImages` 是这次操作涉及的图片数（其它写命令为 0）。前端按 id 增量合并树，不重取整棵树。
- 文件夹写操作在改磁盘或数据库前先申请该子树路径写；读图任务（AI 描述/embedding/姿势、缩略图重建、估算、查看器）只在读原图期间持有短读锁，因此文件夹改名最多等当前这一张读完，不会被整个长任务挡住。读者拿到锁后按 id 重新读取数据库行，路径已变就重试，不会继续写旧路径。
- 路径锁分三级：读、原地替换、写。读与读、读与原地替换都不互斥——原地替换的发布方式是“临时文件 + 原子重命名”，读者要么读到旧图要么读到新图，不会读到半个文件，所以图片空间优化期间查看器、MCP 拼图与复制照常可用；移动、删除、导入与另一个压缩仍是写，与替换互斥。
- 任务与查看器读原图的固定顺序是“先拿文件夹读锁并读字节，释放后再拿图像预算（线程许可在前、字节额度在后）做解码/编码”，不会拿着预算许可去等文件夹锁。查看器、MCP 拼图与复制这类交互式读取在文件夹正忙时直接返回可重试错误，不排在长时间任务后面。
- 文件夹计数由 `get_folders` 用一条直接计数查询在内存中沿路径前缀累加到祖先，结果随命令返回。
- `folders.sort_index` 在新建、移动与排序时维护：新建与移动追加到同级末尾；`reorder_folders` 只接受同一父级的子文件夹，在事务里插入到指定位置并重新编号同级。
- 文件夹树始终按 `sort_index`、id 显示，不再提供常驻的名称/图片数显示模式。右键文件夹（含“全部图片”根行）可做一次性排序：依据为名称或图片数（含子文件夹），可升降序，可勾选“包含所有后代”逐层排序；前端算好每个父级的完整同级顺序，`apply_folder_order` 校验集合后一次写回，只产生一条 `FolderReorder` 记录。
- 直属图片：`LibraryFilters.folderDirect = true` 时，`folderIds` 只匹配直接位于这些文件夹的图片，不展开子文件夹；根目录只匹配根目录直属图片。前端在“有子文件夹且有直属图片”的文件夹展开后显示虚拟行“直属图片 N”，点击后进入单文件夹直属视图；多选文件夹不提供该入口。
- 排序与移动记录标题包含路径：例如“移动文件夹「人物/速写」”“移动 3 个文件夹到「风景」”“把文件夹「草稿」移入回收站（12 张图片）”“恢复文件夹「草稿」”。
- 文件夹展开集合按图库保存在前端 `localStorage`（`lilith.folderExpanded.<libraryId>`，值为相对路径数组）；换库各自恢复，没有记录时展开顶层文件夹。改名或移动后的子树按新路径找不到旧展开项。
- 临时文件夹（`kind='temp'`）不参与文件夹树、计数与同级排序；普通文件夹的创建、重命名、移动、删除与排序都拒绝临时文件夹与 `.artbank-temp*` 路径。

## 临时文件夹

每个图库恰好有一个 `folders.kind='temp'` 行，内部路径为 `.artbank-temp`（同名冲突时追加 `-1`、`-2`）。它是导入的临时落地区，不是普通文件夹：

- 创建或打开图库时确保行与物理目录存在，并清空上一次会话留下的临时图片；关闭图库与应用退出时再次清空。清空会删掉临时图片行、原图与集合引用，保留临时文件夹本身。
- 临时图片只通过导入 `target=temp` 进入；普通导入不能指定临时路径，图片也不能移入或移出临时文件夹。
- 普通视图、全库搜索、文件夹树、标签列表、集合计数与图库计数都不包含临时图片；只有 `view=temp`、显式 id 与 `find_similar` 的 `scope=temp` 能拿到它们。全库相似扫描（`scope=library`）、相似分组（任何范围）、导入查重与库内 pHash 索引同样排除临时图片。
- 临时图片可以查看、评分、改描述、加标签、作为相似参考；不能移入回收站/恢复/永久删除、做几何编辑或加入集合。上述写路径在 Rust 侧统一拒绝。
- 临时图片的评分、描述、标签与 AI 描述不写操作记录，也不会清空重做分支；导入临时文件夹同样不建 `operations` 行。清空临时文件夹不属于撤销范围。

## 操作记录与撤销

历史保存在当前图库的 SQLite（`operations` / `operation_chunks` / `undo_runs`），换库后各自独立；最大步数来自设置 `history.maxSteps`。

- 已接入：移入回收站、恢复、永久删除、清空回收站；文件夹移入回收站（`folderTrash`）、恢复回收站文件夹（`folderRestore`）、永久删除回收站文件夹（`folderPurge`）；导入、编辑保存、图片移动、文件夹创建/重命名/移动/删除/排序；评分、描述（手工与单张 AI）、标签设置/增删/改名/删除/合并（`kind='tagMerge'`）、集合创建/重命名/删除/排序/加入/移出/运算、相似分组批量建集合、批量 AI 描述；图片空间优化（`kind='optimize'`，只记结果，不可撤销）；孤儿移入回收站（`kind='orphanMove'`，可撤销）。缩略图重建、embedding/pose 生成与相似扫描等派生数据不进入记录。
- `orphanMove` 的 `data` 是 `(源绝对路径, 目标绝对路径)[]` 的 JSON 数组，整次清理写**一条**记录，撤销一次移回整批；原路径已被占用时跳过该条并计入「无法恢复」。它不改变图库内容，`releases_files()` 返回 `false`，不参与待释放统计。
- 同步写入用 `history::write`：记录行、逆数据与业务更新在同一个 `IMMEDIATE` 事务里；`affected=0` 且没有逆数据时整条回滚，既不留下空记录，也不清空重做分支（没有实际改动的操作不算新时间线）。`write` 在同一个事务里删除 `undone` / `redoing` 记录：确实写入的新操作开启新时间线，灰色记录与其暂存文件不再可重做。
- 写事务里的文件动作（搬移原图或目录、发布编辑结果、新建目录）通过 `draft.journal()` 记进 `library/fs_journal.rs` 的 `FileJournal`。事务提交成功才 `disarm()`；业务错误、SQL 失败、提交失败与 panic 展开都会在日志析构时逆序撤回，撤回失败只记日志。搬移文件、图片移动、编辑保存、文件夹新建/重命名/移动与孤儿清理都走这一条补偿路径，不再各写一套局部回滚。
- 长任务用 `history::LiveOperation` 包住 `begin_live`：每批在同一个事务里用 `append_live_in_tx` / `add_live_affected_in_tx` 追加逆数据，正常结束、取消、失败与 panic 都通过 `finish_live` 收尾；导入、批量 AI 描述与图片空间优化使用。排队取消或线程起不来时 guard 随闭包丢弃，空记录删除；进程被强杀时 Drop 不执行，仍由下次打开恢复。
- `begin_live` 不结束重做分支：导入可能整批都是重复跳过，或排队未启动；只有确实写入内容的批次（导入批次、AI 描述写入与压缩批次）才调用 `discard_redo_in_tx`。崩溃留下的 `pending` 记录在下次打开时按 `origin_operation_id` 或已有 chunk / `affected` 转为 `applied`，没有内容则删除。撤销/重做运行中 panic 会把 `undo_runs` 标为失败；`undoing` / `redoing` 记录保留给下次打开续做，本次不自动重试。
- 图片空间优化不写任何逆数据：每批事务内更新 `images` 并累加 `affected`，完成后用 `history::update_label` 把「N 张 · 省 X」写进标题。`run_undo` 在标记 `undoing` 之前预检链中含 `optimize` 就返回“压缩操作不可撤销，原图已删除”，不会留下假的 `undoFailed`；记录面板同样禁用撤销。
- 撤销是线性回退：选中第 N 条 = 依次撤回当前仍为 `applied` 的记录，直到 N。成功的记录不再删除，而是标为 `undone`（列表里变灰）；`undone` 记录保持原 chunks，并通过 `redo` chunks 保存撤销前的状态。多条记录的撤销只在记录之间检查取消。
- 重做从当前状态出发，选中的目标是“重做到这里”：先找出目标及下方连续的 `undone` / `redoing` 记录，按 id 升序重新应用。重做前把整批记录标为 `redoing`；崩溃后这些标记就是续做范围，不会顺带重做用户没选中的更新记录。取消或失败时未完成的记录恢复为 `undone`；失败原因保存在 `error`，修正后可再次重做。
- 撤销失败会把记录标为 `undoFailed` 并保留错误；之后不能在它之前继续回退。可以对这条记录单独「重试撤销」，或「跳过」：跳过会把它标为 `undoSkipped`，仍留在时间线里，但不再参与撤销与重做，也不再阻挡更早记录。重做失败或部分重做后，UI 会阻止继续向更早记录撤销，直到重做重试成功（撤销失败则靠重试或跳过解除）。所有逆操作与重做操作都可重复执行。
- 裁剪只处理 `applied` / `undoFailed` / `undoSkipped`，保留最新 `maxSteps` 条；每次成功写入、保存设置、打开图库与导入收尾后各裁剪一次。被裁掉的编辑/导入记录删除 `.artbank/undo/<id>/`，永久删除安排 `ReleaseGarbage`；`undo_runs` 中目标已被裁掉的行同事务清理。
- `ReleaseGarbage` 找 `purge_operation_id` 已没有对应 `operations` 行的批次，按 400 张一块删行、删文件；图片处理完后自底向上删除已无图片、已无子文件夹且磁盘目录为空的回收站文件夹行与目录。可重复执行，也可取消：每 400 行检查一次取消，停下时未处理的行仍带待释放标记，消息写「已停止，剩余待释放保留到下次」。有释放失败的终态是「部分失败」而不是「完成」，失败条数进失败清单与日志。「清空操作记录」只删掉全部记录、`undo_runs` 与暂存目录，不删除任何原图文件；已从回收站移除的图片会留在磁盘上，之后由单独的「释放待删文件」动作删除（自动释放或重新打开图库时也会清理）。
- 释放失败要落到用户看得见的地方：删文件失败与「回收站目录里还有未纳管文件」都写进任务的失败清单与日志，任务消息给出“N 项未能释放”。原图路径无法解析（哈希或扩展名不合法）的行不删行——删了就再也查不到——只清掉待释放标记并保留在回收站，失败清单里给出存库路径。
- 「还有待释放文件」按磁盘事实判断：图片的无主标记只要有就算，文件夹的无主标记要看目录是不是不存在或已空。目录里还有未纳管文件的文件夹保留标记但不算待释放，因此不会每次写入或开库都白排一次释放任务；清空回收站会把这些无主标记改成新记录，下次释放再试。
- 待释放行对查询不可见：图片与文件夹的 `purge_operation_id` 非空时分别从回收站视图、回收站树与全库搜索中排除。图片的待释放行一定有 `deleted_at`，普通视图与集合视图靠 `deleted_at IS NULL` 挡住；`rows_by_ids` 单独加同一条件。不要无条件加这条条件，它会让 20 万行按文件夹排序的首页从 39 ms 涨到 81 ms（见 [性能基准](../guides/performance.md)）。
- 启动恢复在 `AppState::open_library` 做 DB 部分：pending 导入转 `applied` 或删除、收集最小 `undoing` id 与最大 `redoing` id、清理无主暂存目录与统计无主待释放批次；后台任务在 `application.manage` 之后安排，分别续做撤销与重做。
- `HistoryPage`：`{ entries, maxSteps, pendingPurgeCount, pendingPurgeBytes, storage, recoveryNeeded, canRun, blockedReason }`，`canRun` 表示此刻能否开始撤销或重做（没有运行中的任务且列表非空）；`HistoryEntry`：`{ id, kind, label, createdAt, affected, status, error }`，`status` 为 `applied` / `undone` / `redoing` / `undoFailed` / `undoSkipped`。`pendingPurge*` 统计全部待释放行，不区分记录是否仍可撤销。
- `HistoryStorage`：`{ recordCount, chunkCount, chunkBytes, undoFiles, undoBytes, totalBytes }`。逆数据用 `sum(length(data))` 统计，不读 BLOB 内容；`undo*` 递归统计 `.artbank/undo/` 的普通文件并跳过符号链接。操作记录面板显示记录数与占用；清空只删除记录与暂存，不对 SQLite 执行 `VACUUM`，数据库文件不会立即缩小。
- `recoveryNeeded` 为真表示存在 `undoing` / `redoing` 记录且没有对应撤销/重做任务在跑；此时 `blockedReason` 提示重新打开图库恢复。
- 界面入口在浏览工具栏“记录”按钮：灰色 `undone` 记录可选中重做，“当前状态”分隔线标出撤销位置；新操作后灰色记录消失。见 [浏览与检索](browse-and-query.md#操作记录)。

### 逆数据格式与撤销/重做语义

- 原始逆数据写在 `ids` / `triples` / `moves` / `json` / `sortJson` chunks；撤销时另写 `redo` chunks（同一张表，`format='redo'`），只描述“撤销前是什么”，原始 chunks 不动。每次从 `applied` 进入撤销会先清掉旧 `redo` chunks；崩溃续做保留已写入的快照。
- 回收站/恢复：`triples(imageId, oldDeletedAt, oldModifiedAt)`。撤销写回旧状态；重做使用撤销时捕获的 `ImageStates`（当前 `deleted_at` / `modified_at`）。永久删除/清空回收站撤销时先记下 `purge_operation_id` 的图片与文件夹 id，再清空标记；重做重新写标记。
- 文件夹移入回收站：`json` 保存子树 `FolderRow`（id、路径、创建时间、排序位），`triples(imageId, 0, oldModifiedAt)` 保存一起进入回收站的图片。撤销把文件夹行的 `trashed_at` 清空并写回图片旧修改时间；重做重新写同一个时间戳。
- 恢复回收站文件夹：`json` 保存恢复的整个删除单元文件夹行，`triples(imageId, deletedAt, oldModifiedAt)` 保存恢复前的回收站时间戳与修改时间。撤销重新移入回收站并恢复图片状态；重做清空 `trashed_at` 与 `deleted_at`。
- 永久删除回收站文件夹：撤销先记下 `purge_operation_id` 的文件夹 id（`PurgeFolderIds`）与图片 id，再清空标记；重做按原 id 重新写标记。释放任务已经删掉的行无法恢复，这一点由“过期或清空记录后不可撤销”的现有协议保证。
- `moves`：`(imageId, fromFolderId, toFolderId, oldModifiedAt)`。撤销按当前 `folder_id` 判断：已是源文件夹就跳过，仍是目标文件夹才把文件移回并恢复 `modified_at`；文件移动失败会回滚本记录已完成的移动。重做按相反方向执行，并恢复撤销时的修改时间。移动时隐式新建的目标文件夹会在撤销后按空文件夹清理；重做按捕获的完整文件夹行重建。
- 文件夹：创建记录本次新建的全部 `(id, path)`（多级路径会同时新建祖先）；撤销时从深到浅，仅在仍为空时删目录与行，重做用原 id、路径、`created_at`、`sort_index` 补回。重命名/移动记录旧/新路径与旧/新 `sort_index`；撤销与重做都以当前路径判断方向。旧的 `folderDelete`（空文件夹物理删除）记录 `id`、路径、`created_at`、`sort_index`，撤销用原 id 重建行与目录，重做在仍为空时再次删除。排序记录同级发生变化的旧 `sort_index`，撤销逐个恢复，重做使用撤销时捕获的目标排序位。批量移动（`place_folders`）另外写一条 `sortJson`，记录目标父级被挤动的兄弟 `(id, 旧位, 新位)`：撤销恢复旧位、重做恢复新位，不依赖 `redo` 捕获；没有 `sortJson` 的旧记录行为不变。
- 文件夹撤销/重做的磁盘动作是整次性的：一条记录可能包含多条 `FolderPathChange`，全部文件系统动作记在 `library/fs_journal.rs` 的 `FileJournal` 里（已搬移、已删空目录、已新建目录）；任一条失败或事务提交失败时逆序撤回，数据库随事务回滚，磁盘也回到同一起点。单项失败不再留下“行还在、目录已搬走”的半截状态。同步写入里的文件补偿用同一个类型（见下节），只是那条路径由事务成功与否决定是否撤回。
- 编辑：保存时在事务内暂存旧原图到 `.artbank/undo/<operationId>/`、写入编辑前完整内容快照（含 pHash / 主色 / `modified_at`）与本次新哈希。撤销要求当前哈希等于本次新哈希，用暂存文件恢复内容、重新编码缩略图、递增 `thumb_version`、清空 `feature_blobs`，并把新原图移入暂存目录（不删除）；重做使用撤销时捕获的完整内容状态，把新原图移回、恢复行与缩略图。撤销/重做都会删除另一侧的预览缓存；旧图暂存会保留以便再次撤销。
- 导入：新增行带 `origin_operation_id`；chunk 记录新建文件夹、新建标签与合并前快照。撤销按 origin 分块把原图移入 `.artbank/undo/<op>/`、捕获完整行（含标签、集合位置、缩略图指针）后删行，恢复合并项，再删除本次新建且仍为空的文件夹与无引用的标签。重做用原 id 插回行、移回原图、补标签/集合位置并恢复合并后的评分/时间，再重建本次新建的文件夹与标签。
- 导入合并的撤销按「恢复到导入前」口径：同一次导入里同一张图可能被合并多次，每条快照都是合并前的状态，只有最早那份是导入前，所以每张图只应用第一条快照，不会停在中间态。撤销时原图已经不在（或暂存文件被清理）的图片，重做时照常插回行、文件留空并记入失败清单，不阻塞同一条记录里的其它图片；这些行由检查图库按「缺失」报出。
- 评分：`triples(imageId, oldScore, oldModifiedAt)`。撤销恢复旧评分与修改时间，重做使用撤销时捕获的新值。
- 描述：JSON 记录旧描述、`description_source`、`description_model` 与 `modified_at`；手工描述与单张/批量 AI 描述共用同一套撤销。FTS 索引由现有触发器随 `description` 更新。
- 标签：`json` 记录旧关系、实际新增/删除的 `(imageId, tagId)` 与本次新建的标签。撤销恢复旧关系并删除已无引用的新建标签；删除标签的撤销用原 id 重建标签与关系，同名标签已存在时合并关系。重做按 `redo` chunks 重建被删标签、恢复关系或再次删除。合并（`tagMerge`）另记表头 `Merge`、分块的源关系 `Pairs` 与新写到目标上的 `MergeAdded`：撤销先删掉这次加到目标上的关系、再按原 id 重建源标签并插回全部关系，重做反之。
- 集合：创建/运算/相似分组记录完整快照（行 + `(imageId, position)`），撤销删除集合，重做按原 id 与条目重建；重命名、排序、加入、移出记录旧值，撤销恢复名称、`sort_order`、`updated_at` 与条目位置。相似分组一次事务创建全部集合，只产生一条记录。
- 编辑/导入暂存目录不会自动过期；记录裁剪、重做分支被新操作清空或“清空操作记录”时删除，启动时也会清理没有对应 `operations.id` 的目录。清理必须在写连接上执行（`cleanup_undo_dirs` 走 `with_writer`）：编辑保存先在自己的写事务里建好 `.artbank/undo/<id>/`、提交后记录才可见，用读连接枚举会把这种“在建”目录当成无主目录删掉，编辑就永远不可撤销。

## 生命周期

- `create_library`：校验名称/绝对路径/目录状态 → 建目录 → 写清单 → 建库与 schema → 打开 → 确保临时文件夹 → 把路径写入设置。
- `open_library_path`：校验清单/库 → 建连接池 → 校验或迁移 schema → 清空上一个会话的临时图片（新建库则为 0）→ 确保临时文件夹 → 打开（计数为 0，由前端随后拉取）→ 把路径写入设置。空目录或无效目录直接拒绝，需先创建。
- 打开或创建新库成功后才替换旧会话；失败保留旧会话与设置中的旧路径。
- 启动：`open_configured_library` 按 `libraryPath` 自动打开；失败只记日志，前端随后再尝试一次并提示，最终退回欢迎页。
- `close_library`：清空当前图库的临时文件夹，再释放连接、清空 AI 运行时状态与 `libraryPath`，不动其它文件。应用退出时同样尽力清空一次。
- `rename_library`：更新清单名称与当前 `LibraryInfo.name`；不改目录名，也不写设置。
- 应用内没有多库列表与“移除图库”操作；换库由“打开另一个图库”完成。已有图库时换库要重启应用：前端先确认，开库成功后调 `restart_app`（`AppHandle::request_restart`），由事件循环在退出流程里重新拉起进程，下次启动按设置里的新 `libraryPath` 自动打开。前端按 URL 缓存缩略图解码、按库构造模块级状态，热切换要逐项清理才不会串库。
- 导入/AI 任务/文件操作运行时，创建与打开图库会被拒绝；关闭图库在 write 任务进行时拒绝，只有 ai/scan 任务时先取消再关闭。

## 能力边界

能力现状见[当前状态](../planning/current-handoff.md)与各模块文档，本文件只记录契约。

本域没有“计划中但未做”的能力：未实现项与待决策项统一记在[待办与待决策](../planning/todo.md)，本文件只记录已生效的契约。明确不做的：描述导出/回填、特征包压缩回收、应用内旧库迁移。

新增命令在实现时追加到 `src-tauri/src/app/commands.rs` 的注册表，并同步本文件。

## 测试

按改动的检查范围与命令见[验证策略](../guides/validation.md)；性能基准见[性能基准](../guides/performance.md)。

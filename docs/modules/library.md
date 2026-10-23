# 图库与元数据

图库是一个目录加上 `.artbank/` 数据区。应用侧由 `AppState` 持有当前图库会话，元数据由 SQLite 管理。本文件记录 Stage 0 已实现的契约；导入、查询与编辑命令在后续阶段追加。

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
- schema 版本写在 `PRAGMA user_version` 与 `meta.schemaVersion`。当前为 1；版本高于程序支持值时拒绝打开。
- schema 在空库上一次性创建，不做兼容迁移；新结构直接调整。

### 表

| 表 | 作用 |
| --- | --- |
| `meta` | `appFormat`、`schemaVersion`、`libraryId`、`createdAt` |
| `folders` | 图库内文件夹；`path` 为 `/` 分隔相对路径，根目录为 `''` |
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

- `deleted_at IS NULL` 表示正常；非空表示回收站。原图在永久删除前不移动。
- `thumb_segment = 0` 表示还没有缩略图；`thumb_version` 用于 URL 缓存失效。
- `feature_blobs` 只存位置与模型/维度，不存向量本体；避免主库膨胀。
- FTS5 使用 `trigram` tokenizer，支持中文子串；查询少于 3 个字符时回退 `LIKE`（Stage 3 实现）。

### 索引

- `images(deleted_at, score DESC, id DESC)`：评分首屏。
- `images(deleted_at, imported_at DESC, id DESC)`：时间排序。
- `images(deleted_at, file_size DESC, id DESC)`：大小排序。
- `images(folder_id, deleted_at, imported_at DESC, id DESC)`：文件夹页。
- `images(deleted_at) WHERE deleted_at IS NOT NULL`：回收站。
- `image_tags(tag_id, image_id)`：标签反查。
- `collection_items(collection_id, position)`：集合顺序。

## 设置

设置文件由 Tauri `app_config_dir` 下的 `settings.json` 保存，原子替换（临时文件 + rename）。字段：

```text
settingsVersion
libraries[]        { id, name, path, addedAt }
lastLibraryId      后端维护；设置页提交不会覆盖它
ai                 { baseUrl, executablePath, port, autoStart }
performance        { readerConnections, workerThreads, thumbnailCacheMb }
ui                 { theme, gridThumbPx }
```

- 图库路径必须绝对；路径按 Windows 规则去尾斜杠、小写后去重。
- `readerConnections` 1-16；`workerThreads` 为空表示自动；`thumbnailCacheMb` ≥64。
- 设置文件损坏时备份为 `settings.bad-<时间>.json` 并使用默认值，不阻塞启动。

## 命令契约

前端统一经 `src/app/api.ts` 调用；参数使用 camelCase。

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `get_settings` | — | `Settings` |
| `save_settings` | `{ settings }` | `Settings` |
| `inspect_library_path` | `{ path }` | `LibraryInspection` |
| `create_library` | `{ name, path }` | `LibraryInfo` |
| `add_library_path` | `{ name, path }` | `LibraryInfo` |
| `open_library` | `{ libraryId }` | `LibraryInfo` |
| `open_library_path` | `{ path }` | `LibraryInfo` |
| `close_library` | — | `null` |
| `get_library_status` | — | `LibraryInfo \| null` |
| `remove_library` | `{ libraryId }` | `Settings` |
| `rename_library` | `{ libraryId, name }` | `Settings` |
| `window_minimize` / `window_toggle_maximize` / `window_is_maximized` / `window_close` | — | 窗口控制 |
| `get_app_info` | — | `{ version, settingsPath }` |

`LibraryInspection`：

```text
{ kind: "library", manifest }
{ kind: "empty" }
{ kind: "invalid", reason }
```

`LibraryInfo`：`id`、`name`、`path`、`imageCount`、`trashCount`、`schemaVersion`、`openedAt`。

## 生命周期

- `create_library`：校验名称/绝对路径/目录状态 → 建目录 → 写清单 → 建库与 schema → 加入设置 → 打开。
- `open_library`：校验清单/库 → 建连接池 → 校验 schema → 读计数 → 加入设置并记录 `lastLibraryId`。
- 打开新库成功后才替换旧会话；失败保留旧会话。
- `close_library`：释放连接并清空 `lastLibraryId`，不动文件。
- `remove_library`：只从设置移除；若正打开则先关闭。不删除磁盘内容。
- `rename_library`：更新设置名称与清单名称；不改目录名。

## 已实现与未实现

已实现：上述生命周期、schema v1、连接池、设置、窗口命令、Stage 0 UI。

未实现（后续阶段）：图片导入、查询/筛选/排序、元数据编辑、集合、回收站操作、缩略图包读写、feature pack、AI 任务。对应命令在实现时追加到 `src-tauri/src/app/commands.rs` 的注册表，并同步本文件。

## 测试

`cargo test` 覆盖：

- 设置往返、重复路径拒绝、后端维护 `lastLibraryId`；
- schema 初始化、FTS 插入/更新、重复初始化拒绝；
- 目录规范校验；
- 图库创建/重开/重复创建拒绝/普通目录拒绝打开。

性能基准（默认忽略）见 [性能基准](../guides/performance.md)。

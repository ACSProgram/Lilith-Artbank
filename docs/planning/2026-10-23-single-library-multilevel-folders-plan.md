# 单库与多级文件夹重构计划

状态：阶段 A–C 已实现并通过自动验证（2026-10-24）；阶段 D（迁移器与试点）未开始，阶段 E 的真实窗口验收待用户执行  
日期：2026-10-23

## 0. 已确认决策

- **单库模式**：应用只维护一个统一图库，不再保留图库列表和并列切换；旧库成为统一库内的顶层文件夹。
- **原地统一 + BLAKE3 改名**：在 `G:\Lilith Image Library` 原地创建 `.artbank`，旧原图从旧 pHash 文件名改为 `<BLAKE3>.<ext>`；不复制数百 GB。
- **按内容去重**：同一 BLAKE3 只保留一个图片记录和一个主文件夹位置；标签取并集、评分取较高值、描述按规则合并。
- **完整文件树管理**：任意层文件夹、树形导航、面包屑、新建/重命名/移动/删除空文件夹、图片拖入文件夹、导入保留源目录层级、筛选默认包含子文件夹。
- **迁移先试点后全量**：先用 `G:\Lilith Image Library\迁移测试` 的小规模副本验证迁移器，通过后再对全部旧库执行。
- **交互参考**：树的选择、展开、拖放、右键菜单可参考 `F:\Programs\Lilith Artworks` 的 `src/modules/library/LibraryTreeView.tsx` 与 `tree.ts`；但 Artbank 的文件夹是真实磁盘目录，不能用 Artworks 的纯数据库节点模型直接替代。

## 1. 现状核对

### 1.1 旧数据

`G:\Lilith Image Library` 下目前是旧程序的多个独立图库：

```text
人体/  参考/  文化/  测试/  画作/  设计/  迁移测试/
```

- 每个目录有自己的 `.data/`、`output/`、`recycle/`，元数据在各自的 `image_data.bin` / `saved_lists.dat` / `embeddings.bin`。
- 旧图库内的文件夹只有一层，例如 `画作/画作：二次元/<pHash>.jpg`。
- 旧图片文件名是 64 位 pHash，`image_data.bin` 保存 `hash / extension / folder / import_time / score / colors / tags / description / pose`。
- 旧 `import_time` 是秒；新库使用毫秒。
- 旧 embedding 是 1024 维 CLIP，和新库的 SigLIP2 1536 维不兼容；旧缩略图是 BC1 DDS，浏览器不能直接复用。
- 旧 `recycle/` 中的文件不在 `image_data.bin` 的活动记录里；本次迁移不读取旧回收站。

### 1.2 新应用

- 新应用仍是“多库列表 + 单库会话”：设置里保存 `libraries[]`，每个图库独立 SQLite、独立缩略图包、独立 AI 特征包；查询、标签、集合、语义搜索都只作用于当前库。
- 元数据只走 SQLite，页面使用键集游标，不会整库载入内存或一次性发给前端。
- 因此，旧程序“必须拆库减少加载量”的前提已经不存在；合并为单库在查询架构上可行。

### 1.3 多级文件夹的已有基础

- `folders.path` 已经是 `/` 分隔的图库内相对路径，根目录为 `''`。
- `normalize_folder` 已支持 `人物/速写` 这类嵌套路径，并拒绝绝对路径与 `..`。
- 导入目标文件夹和 `move_images_to_folder` 都接受嵌套路径，原图会落到 `<root>/<folder>/<hash>.<ext>`。
- 当前真正缺的是：
  - 树形 UI、面包屑、文件夹选择器；
  - 文件夹新建/重命名/移动/删除；
  - 图片拖入文件夹、文件夹拖入文件夹；
  - 文件夹筛选的子树语义与子树计数；
  - 导入时保留源目录层级；
  - 空文件夹、大小写冲突、Windows 保留名等一致性规则。

## 2. 目标与非目标

### 目标

- 一个统一库覆盖原来的画作、参考、人体、文化、设计等全部内容。
- 原来每个旧库成为统一库的顶层文件夹，旧库内的一级文件夹成为第二层。
- 任意层文件夹可创建、重命名、移动、拖放；文件夹与磁盘目录保持一致。
- 文件夹筛选、搜索、集合、回收站、查看器、AI 任务在统一库内正常工作。
- 旧库的标签、评分、描述、集合顺序、导入时间尽量保留。
- 迁移可试点、可报告、可回滚，不静默丢数据。

### 非目标

- 不做跨库聚合视图；单库模式替代多库列表。
- 不做虚拟文件夹；文件夹继续对应真实磁盘目录。
- 不保留旧 CLIP embedding；需要时用新 AI 服务重算。
- 不把旧 `.data` 读取逻辑放进应用；迁移器是仓库外的一次性工具。
- 本次不实现手动兄弟排序；文件夹按名称排序。
- 本次不实现“删除非空文件夹并递归移入回收站”；先只删除空文件夹，避免把物理目录和软删除语义混在一起。
- 不做 OLE 拖出、跨库迁移 UI 等已有待办之外的功能。

## 3. 目标架构

### 3.1 单库生命周期

- 设置只记录一个图库路径，不再维护图库数组。
- 启动时自动打开该路径；路径缺失或损坏时进入欢迎页并提示，不阻塞启动。
- 欢迎页提供“创建新图库 / 打开现有图库”；打开另一个图库视为替换当前库，不做并列列表。
- 设置页显示当前图库名称、路径和计数；提供“更换图库”，不再有移除/重命名列表项的语义。
- `AppState` 仍只持有一个 `LibrarySession`；导入、AI 任务、维护守卫、缩略图与原图协议保持现有模型。
- 所有“切换图库”仍与导入、文件操作、AI 任务互斥。

### 3.2 统一后的存储布局

```text
G:\Lilith Image Library\
  .artbank\                    新库数据区
    library.json
    library.db
    thumbs\
    features\
    temp\
    migration\                 迁移日志、报告、隔离的重复文件
  recycle\                     新库回收站
  output\
  画作\
    画作：二次元\
      <blake3>.jpg             旧图改名后的新文件名
    游戏：二次元\
      ...
  参考\
    场景\
      ...
  人体\
    ...
  迁移测试\                    试点数据；全量迁移时排除
  ...                          旧库的 .data / output / recycle 保留原位
```

- 旧库的 `.data`、`output`、`recycle` 不迁移、不删除；保留为旧程序和人工回溯的备份。
- 扫描统一库根时额外跳过 `.data`；`.artbank`、`recycle`、`output` 仍按现有规则跳过。
- 新库自己的 `.artbank` 只在根目录出现一次；旧库子目录里的 `.data` 不进入文件夹表。
- 新导入的图片仍按内容哈希命名；迁移后的旧图也统一改为内容哈希命名。
- `.artbank/migration/` 不属于用户文件夹，扫描、树和查询都忽略。

### 3.3 文件夹模型

继续使用路径模型，不引入虚拟节点表：

- `folders.path`：`/` 分隔的相对路径；根为 `''`。
- 所有祖先文件夹必须有对应的 `folders` 行；例如创建 `人物/速写/手部` 时，`人物`、`人物/速写` 也要存在。
- 文件夹与物理目录一一对应；空文件夹也保留 DB 行和物理目录，便于树形管理。
- 文件夹按名称排序，不保存手工顺序。
- 文件夹路径在 Windows 下大小写不敏感；新增唯一索引防止 `Foo` 与 `foo` 同时存在。
- 保留目录名：`.artbank`、`recycle`、`output` 不允许作为用户文件夹；`recycle` / `output` 在任意层级都保留，避免和旧库目录或应用数据混淆。
- 文件夹名校验补充 Windows 规则：拒绝 `<>:"|?*`、控制字符、结尾空格/点、`CON`/`PRN`/`AUX`/`NUL`/`COM1-9`/`LPT1-9` 等设备名。
- 树由 `get_folders` 的扁平路径列表在前端构建，不在后端递归拼 JSON；文件夹数量上限按路径数控制，不随图片数增长。

### 3.4 查询语义

- `filters.folderIds` 的含义改为**包含选中文件夹及其全部后代**。
- 子树匹配使用路径边界：

```sql
folders.path = ?1
OR folders.path LIKE ?2 ESCAPE '\'   -- ?2 = 转义后的 path || '/%'
```

- 必须转义 `%`、`_`、`\`，并保证 `人物` 不匹配 `人物速写`。
- 分页、搜索、集合、回收站、标签分面都沿用现有查询构造；只在文件夹条件处改为子树条件。
- `FilterCounts.folders[].count` 改为该文件夹的子树总数；分面仍然排除文件夹自身筛选条件。
- `get_folders` 返回：
  - `id`、`path`、`directImageCount`（直接位于该文件夹的图片数）、`totalImageCount`（含后代）。
  - 两个计数都只统计未删除图片；空文件夹也返回，计数为 0。
- 前端树的总数可以用 `totalImageCount` 直接显示；如果筛选面板打开，可用分面计数覆盖显示。
- 文件夹路径搜索继续使用 `LIKE`，因为 `folders.path` 已包含完整层级，搜索父目录名天然命中后代。
- 集合与标签不因文件夹层级改变；图片仍然只有一个 `folder_id`。

### 3.5 前端结构

- 应用外壳从“图库列表 + 主区”改为“文件夹树侧栏 + 主区”。
- 左侧树：
  - 根节点“全部图片”清空文件夹筛选；
  - 文件夹节点支持展开/折叠、单选、Ctrl/Cmd 多选、Shift 区间选择；
  - 拖放：图片拖到文件夹 = 移动图片；文件夹拖到文件夹 = 移动文件夹；
  - 右键菜单：新建子文件夹、重命名、移动到、删除空文件夹、将选中图片移入此文件夹。
- 主区：
  - 工具栏下方或上方增加当前文件夹面包屑；点击祖先节点切换筛选；
  - 原有“全部 / 回收站 / 集合”视图栏保留；
  - 文件夹筛选进入 `BrowseState.filters.folderIds`，因此浏览历史、搜索范围、选择清理和现有查询刷新逻辑可以复用。
- `BrowseState` 从 `LibraryView` 内部提升到 `LibraryWorkspace` 或独立的 `useLibraryBrowse`：
  - 树和网格共享同一份状态，避免两个 `folderIds` 来源；
  - `LibraryView` 接收受控状态与回调；
  - 树本身放在 `src/modules/library/`，由 `src/app/` 组合，不产生模块间互相导入。
- 筛选面板移除“文件夹”勾选列表，避免和树重复；文件夹筛选统一由树和面包屑表达。状态栏仍显示当前文件夹筛选数量。
- 导入弹窗和批量移动弹窗增加文件夹树选择器；保留文本框作为高级输入。
- 浏览器预览 mock 增加嵌套文件夹、空文件夹和树操作，保证无 Tauri 时仍可检查布局与交互。

### 3.6 导入

- `ImportRequest.folder` 仍是目标根文件夹。
- 新增 `folderMode: "flat" | "preserve"`，默认 `flat`：
  - `flat`：所有图片进入目标文件夹；
  - `preserve`：每个输入目录下的相对路径保留到目标文件夹下；文件输入直接进入目标文件夹。
- 扫描结果需要携带相对目录，而不是只有绝对文件路径。
- 目标文件夹及其祖先由 `ensure_folder` 一次性补齐；物理目录用 `create_dir_all`。
- 同一批导入中，不同源目录同名子目录合并到同一目标路径；文件名不保留，仍按内容哈希命名。
- 单次导入上限、取消、进度、失败清单、重复策略保持不变。

## 4. 契约变更

### 4.1 设置

`settingsVersion` 从 1 升到 2：

```text
Settings {
  settingsVersion: 2,
  libraryPath: string | null,
  ai, performance, ui
}
```

- 移除 `libraries[]`、`lastLibraryId`、`LibraryEntry`。
- v1 → v2：优先取 `lastLibraryId` 对应条目，否则取第一条；写入 `libraryPath`；其余条目丢弃。
- 当前图库名称以 `.artbank/library.json` 为准；设置不保存名称。
- `lastLibraryId` 的旧语义由 `libraryPath` 替代。

### 4.2 Schema v2

- `SCHEMA_VERSION` 从 1 升到 2。
- v1 → v2 迁移：
  - 检测并报告大小写冲突的 `folders.path`；
  - 增加大小写不敏感唯一索引：

```sql
CREATE UNIQUE INDEX IF NOT EXISTS idx_folders_path_nocase
  ON folders(path COLLATE NOCASE);
```

- `images`、`tags`、`collections`、`thumb_segments`、`feature_blobs`、`image_search` 结构不变。
- `folders.path` 的现有唯一索引保留；NOCASE 索引只负责 Windows 大小写语义。
- 新库直接按 v2 建表；旧 v1 库在打开时执行上述轻量迁移。
- 不在本次重构中改动 `images` 的唯一内容哈希约束、单文件夹外键或缩略图指针。

### 4.3 命令

生命周期：

| 命令 | 变化 |
| --- | --- |
| `get_settings` / `save_settings` | 使用 v2 设置 |
| `inspect_library_path` | 保留 |
| `create_library` | 保留；创建后写入 `libraryPath` |
| `add_library_path` | 改为 `open_library_path`：打开路径并替换当前库 |
| `open_library` | 移除按 id 打开 |
| `close_library` | 保留；关闭后清空运行时与 `libraryPath` |
| `get_library_status` | 保留 |
| `remove_library` | 移除 |
| `rename_library` | 改为重命名当前库的清单名称 |

文件夹：

| 命令 | 输入 | 输出 | 说明 |
| --- | --- | --- | --- |
| `get_folders` | — | `FolderInfo[]` | 扁平列表，含空文件夹与祖先 |
| `create_folder` | `{ path }` | `FolderInfo[]` | 创建路径及缺失祖先 |
| `rename_folder` | `{ folderId, name }` | `FolderInfo[]` | 只改叶子名称，更新后代路径 |
| `move_folder` | `{ folderId, parentId }` | `FolderInfo[]` | 移动到另一个文件夹或根 |
| `delete_folder` | `{ folderId }` | `FolderInfo[]` | 仅空文件夹 |
| `move_images_to_folder` | `{ imageIds, folderPath }` | `MoveReport` | 保留；目标路径会创建祖先 |

查询与导入：

- `LibraryQuery.filters.folderIds` 语义改为子树，字段名不变。
- `FolderInfo` 增加 `directImageCount`、`totalImageCount`；`imageCount` 如保留则作为兼容别名，重构完成后删除。
- `ImportRequest` 增加 `folderMode`。
- 导入扫描 DTO 增加每个文件的 `relativeFolder`（内部结构，不发给前端）。

### 4.4 文件夹操作语义

所有写操作共用维护守卫，拒绝导入、AI 任务或图库切换并发。

- **创建**：校验路径；创建物理目录；按从根到叶的顺序插入缺失的 `folders` 行；返回完整列表。
- **重命名**：只接受单段名称；拒绝保留名与非法字符；目标目录不能已存在（大小写不敏感）；Windows 大小写改名使用临时名中转。
- **移动**：目标父文件夹必须存在；不能移动到自身或后代；目标路径不能已存在；先 `rename` 物理目录，再在一个事务里更新该文件夹及全部后代的 `path`；失败时把目录移回。
- **删除**：只允许没有图片、没有子文件夹且物理目录为空的文件夹；删除 DB 行后删除空目录。非空文件夹在 UI 中禁用并给出原因。
- **图片移动**：沿用现有逐张 `rename` + `folder_id` 更新与失败回滚；目标文件夹由 `ensure_folder` 补齐祖先。
- **重命名/移动后的缓存**：缩略图、原图、AI 特征都按图片 id/内容哈希，不因路径变化失效；语义缓存不失效。
- **浏览状态**：文件夹 id 稳定；重命名/移动后树刷新但选中 id 不变；删除空文件夹时若正被选中，清空文件夹筛选。

## 5. 全量迁移

### 5.1 迁移器边界

- 旧 `.data` 读取不进入 Tauri 应用；迁移器是仓库外的一次性工具。
- 迁移器输出符合本计划的新 `.artbank` 结构；应用只负责打开结果。
- 旧格式参考旧程序源码中的 `ImageLibraryContext.cpp` / `ImageLibraryCore.cpp`；压缩格式沿用旧程序的压缩/解压约定。
- 迁移器必须有 `--dry-run`、进度、失败清单、已处理计数、可重复执行与重跑续传。

### 5.2 试点

- 试点内容：`G:\Lilith Image Library\迁移测试` 的副本，放在临时目录；不在真实库上直接写。
- 复制时只需原图、`image_data.bin`、`saved_lists.dat`；`previews.pak`、`previews.idx`、`embeddings.bin` 可省略。
- 试点目录：
  - 统一库根为临时目录；
  - 顶层文件夹为 `迁移测试`；
  - 其下保留旧库的一级文件夹结构。
- 试点通过标准：
  - 旧活动记录与迁移记录数量一致，缺失文件有清单；
  - 每张图可打开，缩略图可显示；
  - 标签、评分、描述、导入时间抽查一致；
  - 集合名称与成员顺序抽查一致；
  - 重复内容只保留一个主位置，隔离清单可读；
  - 文件夹树、面包屑、子树筛选、计数正确；
  - 迁移可中断并重跑，不重复插入。

### 5.3 全量迁移范围

- 真实根：`G:\Lilith Image Library`。
- 迁移前确认根目录没有 `.artbank`；若有，停止并人工处理。
- 迁移库清单：
  - `画作`、`参考`、`人体`、`文化`、`设计`、`测试`；
  - `迁移测试` 只用于试点，全量迁移时排除，避免重复。
- 旧根下的 `desktop.ini`、Python 脚本、`output/`、`recycle/`、各旧库的 `.data/` 不迁移。
- 顶层文件夹名使用旧库目录名；旧库内的 `folder` 字段拼接为 `库名/旧文件夹`。旧 `folder` 为空时使用 `库名`。
- 旧 `recycle/` 不迁移；保留原目录，后续如需恢复旧回收站再单独处理。

### 5.4 单文件迁移流程

```text
读取旧记录
  → 校验 hash / extension / folder / 文件存在
  → 计算 BLAKE3
  → 检查是否已有相同内容
  → 生成目标路径 <库名>/<旧文件夹>/<blake3>.<ext>
  → 校验目标路径无冲突
  → 原子 rename 旧文件到新文件
  → 解码原图，重新计算宽高/格式/alpha/pHash/主色
  → 生成 512px WebP 缩略图并追加到新 pack
  → 写入 images 行、folders 行、tags、image_tags
  → 写迁移日志
```

- `imported_at = 旧 import_time × 1000`。
- `score` 保留 0-5；超出范围截断并报告。
- `tags` 按新库规则去重；标签名超长或非法时报告。
- `description` 保留原文；`description_source` 设为 `user`，避免后续 AI 批量任务覆盖。
- `width/height/format/has_alpha/file_size` 以实际解码结果为准。
- `pHash` 与主色重新计算，不直接复用旧值，确保新相似度算法一致。
- 缩略图重建，不转换旧 DDS。
- 旧 pose 可选迁移：按旧 33 点数据写入新 feature pack；如果格式校验失败，记录失败并保留旧 `.data`，不阻塞迁移。
- 旧 embedding 不迁移。
- 删除旧记录中的 `delete_mark` 项（旧保存时已跳过，正常不会出现）。

### 5.5 重复内容

- 以 BLAKE3 为唯一内容键。
- 同一内容首次出现为主记录；后续重复记录：
  - 标签取并集；
  - 评分取较高值；
  - 描述：保留第一个非空描述，其他非空冲突写入报告；
  - 集合并入主记录；
  - 导入时间取较早值；
  - 文件不保留第二份，移动到 `.artbank/migration/duplicates/<库名>/...`，并写入清单。
- 主文件夹选择顺序由迁移配置的库顺序决定；默认按旧库目录名排序，再按文件夹路径、旧导入时间、文件名排序，保证可重复。
- 迁移报告必须列出：重复内容、主记录、被隔离路径、合并的标签/集合、描述冲突。

### 5.6 集合

- 旧 `saved_lists.dat` 转为 `collections` / `collection_items`。
- 保留列表名称、顺序和成员顺序。
- 成员按旧 hash → 新图片 id 映射；缺失或已被去重的成员跳过并报告。
- 名称冲突时追加 `（库名）`；仍冲突时追加序号。
- 空集合保留；旧列表名称非法或为空时报告并按“未命名集合”处理。

### 5.7 回滚与安全

- 迁移器在改名之前写 `rename-journal.jsonl`，记录 `old_path`、`new_path`、BLAKE3、时间。
- 旧 `.data` 不改；旧文件名映射完整保留。
- 回滚工具按日志反向 rename；若目标路径已被占用则逐条报告，不强行覆盖。
- 迁移前检查磁盘剩余空间；新缩略图/特征包需要额外空间，原图不复制。
- 迁移期间旧程序必须关闭；迁移器检测文件占用并报告。
- 全量迁移完成后保留一份 `.artbank/library.db` 备份与迁移报告。

## 6. 性能与测试

### 6.1 性能门槛

沿用现有预算，并补充文件夹树路径：

| 路径 | 预算 |
| --- | --- |
| 首屏 150 | ≤50 ms |
| 子树文件夹页 150 | ≤50 ms |
| 文件夹分面计数（含子树聚合） | ≤100 ms |
| `get_folders`（1 万个文件夹） | ≤50 ms |
| 文件夹重命名/移动（1 千个子文件夹） | 可接受的一次性操作，不阻塞 UI 线程 |
| 缩略图一页 150 张 | ≤1 s（硬上限 3 s） |

- 用 20 万行合成库验证；文件夹树与子树条件不能退化为全表扫描。
- `folders` 数量增加时，子树展开只需查询 `folders`，不扫描图片。
- 导入保留层级时缓存文件夹 id，避免每张图重复查路径。

### 6.2 自动测试

Rust：

- 设置 v1 → v2 迁移、单库自动打开、旧列表条目选择。
- schema v1 → v2、大小写冲突检测、NOCASE 索引。
- 文件夹路径规范化、Windows 保留名、非法字符、祖先补齐。
- 创建/重命名/移动/删除空文件夹；移动失败回滚；禁止移动到自身或后代；禁止删除非空。
- 子树查询：父目录包含后代、`人物` 不匹配 `人物速写`、空文件夹、搜索/集合/回收站组合。
- 文件夹分面计数与直接计数一致；总数不重复计算嵌套文件夹。
- 导入 `flat` / `preserve` 两种模式、多目录同名子目录合并、取消与失败清单。
- 图片移动到嵌套目标、目标祖先补齐、失败回滚。
- schema 外键、删除文件夹后的图片引用完整性。

前端：

- `npm run typecheck`、`npm run build`。
- 浏览器预览：树展开/折叠、多选、面包屑、空文件夹、拖放、右键菜单、筛选后的计数、集合/回收站视图。
- 不自动驱动真实 Tauri 窗口；最终界面由用户手工验收。

### 6.3 迁移测试

- 迁移器单元测试：旧格式解压、字段解析、时间换算、扩展名规范化、hash 去重、集合映射、日志回滚。
- 试点副本端到端；对比旧记录数、抽样元数据、缩略图、树结构。
- 全量迁移后执行 SQLite `PRAGMA integrity_check`、内容哈希唯一性检查、文件夹孤行检查、图片文件存在性抽样。

## 7. 实施顺序

### 阶段 A：契约与 schema

1. 冻结设置 v2、schema v2、子树语义、文件夹命令与 DTO。
2. 更新 `docs/modules/library.md`、`browse-and-query.md`、`import.md`、`overview.md` 的目标段落；实现前先以计划为准。
3. 添加设置与 schema 迁移测试。

### 阶段 B：后端单库与文件夹

1. 设置 v2、单库生命周期、命令注册表清理。
2. `folders` 祖先补齐、路径校验、大小写唯一索引。
3. 文件夹创建/重命名/移动/删除空文件夹。
4. 查询子树条件、`get_folders` 计数、分面计数。
5. 导入 `folderMode` 与扫描相对路径。
6. Rust 单元测试与查询基准。

### 阶段 C：前端单库与树

1. 应用外壳、欢迎页、设置页、图库对话框改为单库。
2. 文件夹树、面包屑、文件夹选择器、右键菜单、拖放。
3. `BrowseState` 提升与树/网格同步；筛选面板调整。
4. 导入/批量移动复用树选择器。
5. 浏览器 mock 与 typecheck/build。

### 阶段 D：迁移器与试点

1. 仓库外迁移器：旧格式解析、BLAKE3、改名、缩略图、DB 写入、报告、日志。
2. 在 `迁移测试` 副本上跑试点，修复问题。
3. 试点通过后，经用户确认再对 `G:\Lilith Image Library` 全量执行。
4. 迁移后验收与备份。

### 阶段 E：收尾

1. 跑完整验证：`npm run typecheck`、`npm run build`、`cargo fmt --check`、`cargo check`、`cargo clippy --all-targets`、`cargo test --lib`、`git diff --check`。
2. 更新架构/模块/性能文档；把多库待决策项从 `todo.md` 移除或标记已决策。
3. 用户手工验收真实 Tauri 窗口；代理不自动打开窗口做输入自动化。

## 8. 预计影响文件

Rust：

```text
src-tauri/src/app/settings.rs
src-tauri/src/app/state.rs
src-tauri/src/app/commands.rs
src-tauri/src/app/mod.rs
src-tauri/src/library/schema.rs
src-tauri/src/library/paths.rs
src-tauri/src/library/service.rs
src-tauri/src/library/import.rs
src-tauri/src/library/query.rs
src-tauri/src/library/metadata.rs
src-tauri/src/library/db.rs
src-tauri/src/import/mod.rs
src-tauri/src/import/scan.rs
src-tauri/src/import/pipeline.rs
```

前端：

```text
src/app/AppShell.tsx
src/app/AppRuntimeProvider.tsx
src/app/Sidebar.tsx
src/app/LibraryDialog.tsx
src/app/WelcomeView.tsx
src/app/SettingsDialog.tsx
src/app/LibraryWorkspace.tsx
src/app/api.ts
src/app/types.ts
src/modules/library/types.ts
src/modules/library/api.ts
src/modules/library/model.ts
src/modules/library/LibraryView.tsx
src/modules/library/FilterPanel.tsx
src/modules/library/Inspector.tsx
src/modules/library/mock.ts
src/modules/library/FolderTree.tsx         新增
src/modules/library/FolderDialogs.tsx      新增
src/modules/library/FolderPickerDialog.tsx 新增
src/modules/library/folderTree.ts          新增
src/modules/import/ImportDialog.tsx
src/modules/import/types.ts
```

迁移器：

```text
仓库外独立工具；不进入 Tauri 应用与 src-tauri。
```

## 9. 风险与未决

- **原地改名不可逆感**：旧程序将无法按原文件名打开；必须保留 `rename-journal.jsonl` 与旧 `.data`，并提供回滚工具。
- **全量迁移 I/O 时间长**：需要对数百 GB 原图做 BLAKE3、解码、缩略图与 pHash；迁移器要支持分批、进度、取消、重跑。
- **重复内容归属**：主文件夹按迁移顺序选择，可能与用户直觉不同；报告必须清楚，必要时允许人工调整主记录。
- **非空文件夹删除**：本次只删除空文件夹；如果用户需要“删除文件夹并递归移入回收站”，需要单独设计文件移动与恢复语义。
- **旧回收站**：本次不迁移；旧 `recycle/` 保留。若用户需要恢复旧回收站，另做一次性脚本。
- **旧 pose**：格式可迁移则迁；失败只报告，不阻塞主流程。
- **旧集合重名**：自动加后缀；用户可能希望人工指定合并规则，先按可逆后缀处理。
- **大小写唯一**：SQLite `NOCASE` 只覆盖 ASCII；中文不受影响，非 ASCII 大小写冲突仍需迁移前检查报告。
- **文件夹树与筛选面板重复**：计划让树成为唯一文件夹筛选入口；如果用户仍希望保留多选勾选列表，可在后续加“高级筛选”面板。
- **统一库名称**：默认沿用根目录名或“Lilith Artbank”，实现时可在迁移报告中确认；不影响数据路径。

## 10. 完成标准

- 应用只维护一个图库；启动、创建、打开、更换、关闭路径行为明确。
- 任意层文件夹可创建、重命名、移动、删除空文件夹；物理目录与 DB 一致，失败可回滚。
- 树、面包屑、拖放、右键菜单、文件夹选择器可用；文件夹筛选包含子树，计数正确。
- 导入可选择保留源目录层级；批量移动可选择树节点。
- 20 万行合成库上，子树查询与文件夹计数满足预算。
- 迁移器在 `迁移测试` 副本上通过试点；全量迁移后旧标签、评分、描述、导入时间、集合顺序保留；重复内容有隔离与报告。
- 旧 `.data` 不被应用读取；迁移器在仓库外；真实库在用户确认后才写入。
- 自动检查全部通过；真实 Tauri 界面由用户手工验收。

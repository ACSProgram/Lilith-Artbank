# MCP 图库控制

支持 MCP 的 AI 客户端通过应用内服务读取并修改当前图库。批次 1（只读）、批次 2（写工具、撤销/重做、询问档、任务工具）、批次 3（导入、导入回执、`logs/mcp.log` 审计）、批次 4（找相似范围与平均参考）、批次 5（临时文件夹）与批次 6（导入归因、临时区特征生成）已实现；使用反馈批次 5 补了列表分页、规则自描述、标签缺失/排除筛选与标签管理写入。

传输与进程：

```text
MCP 客户端 (stdio) → lilith-artbank.exe --mcp-stdio
                     → 127.0.0.1:<port>/mcp（Bearer 口令）
                     → mcp::server → AppState → library/ai/history

MCP 客户端 (Streamable HTTP) → 应用内 HTTP 服务（同一地址）
```

- MCP 服务在应用进程内运行，工具直接调用 `AppState`，不经过 Tauri IPC，不另起图库写进程。
- `--mcp-stdio` 在 Tauri 初始化与 single-instance 插件之前处理；stdout 只写 MCP JSON-RPC，诊断写入 `logs/mcp.log`。
- HTTP 只绑定 `127.0.0.1`，每个请求都必须带 `Authorization: Bearer <token>`。
- 服务启动时生成随机口令，把配置端口与口令写入程序根目录的 `mcp-runtime.json`。stdio 小桥每次请求前读取它；应用重启后会自动换用新口令。
- 设置页复制的 stdio 配置使用 `get_app_info` 返回的当前进程路径（`exePath`）：开发模式指向 `src-tauri/target/debug/`，便携模式指向 exe 所在目录。不要手工拼“设置文件目录 + exe 名”，开发模式下该目录没有可执行文件。
- 直接配 Streamable HTTP 的客户端必须使用设置里的端口，并在每次应用重启后从 `mcp-runtime.json` 取新口令、更新 `Authorization`；端口或口令过期会表现为连接被拒或 401。
- 服务默认关闭；设置中开启后使用配置端口。开关或端口变化会重启服务并换口令；只切换档位时保留监听与口令，已连接的客户端不会断线。设置页保存后立即生效。
- 挂起的待确认请求只存在内存里；应用退出后清空，换库/关库、关闭服务或切到只读时也会清空。

## 设置与三档行为

设置 v6 增加：

```text
mcp { enabled: false, port: 18767, mode: "readOnly" | "ask" | "full" }
```

- `enabled` 默认 `false`；`port` 默认 `18767`，范围 1-65535。
- `mode` 默认 `ask`，工具按当前设置实时判定；运行时文件里的 `mode` 只是标记，切档不重新生成口令。
- 端口被占用等启动失败只记录在 `get_mcp_status` 的错误里，不回滚设置。
- 设置页 MCP 分类显示开关、端口、三档、运行状态、待确认列表、最近 AI 操作、完全档警告与 stdio 客户端配置复制。

| 档位 | 行为 |
| --- | --- |
| `readOnly` | 写工具、`import_start`、`history_undo` / `history_redo` 返回“当前是只读模式”；只读工具与 `history_list`、`tasks_get` 可用。 |
| `ask` | 导入、回收站、撤销/重做、文件夹移动与删除、标签删除与合并、以及**影响超过 100 张**的批量写入进入待确认队列；其余写入直接执行（含标签重命名）。 |
| `full` | 不弹应用内确认，直接执行；`confirmedCount`、撤销/重做数量核对仍然保留。 |

档位事实以 `library_info.mcp` 为准：`writeAllowed` 表示当前档位能否写入，`approval.threshold` 只在询问档给出（其它档为 `null`），`approval.alwaysApproveInAsk` 列出询问档无条件要确认的工具。100 张阈值不是设置项，也不在询问档之外生效。

只读档不隐藏写工具，客户端缓存的 `tools/list` 不会因为切换档位失效。

## 工具（24 个）

### 只读（8）

| 工具 | 输入 | 返回 |
| --- | --- | --- |
| `library_info` | — | 图库、普通/回收站/临时计数、schema、特征数量、AI 配置、当前 MCP 档位（`mode` / `writeAllowed` / `approval`）与限制 |
| `folders_list` | `parentPath?`、`limit?`、`cursor?` | 直接子文件夹、路径、直接/子树计数、`total`、`nextCursor` |
| `tags_list` | `query?`、`limit?`、`cursor?` | 有图片的标签 id、名称、图片数、`total`、`nextCursor`；0 张标签不返回 |
| `collections_list` | — | 集合 id、名称、数量、顺序 |
| `search_images` | 结构化查询（`view` 可含 `temp`）、`pageSize` 1–50、`searchScope`、`includeFacets?` | 总数、紧凑行、下一页游标；`search` 同时匹配描述、标签名与文件夹名；`view` 只有 all/trash/collection/temp，跨回收站搜索用 `searchScope=library` |
| `find_similar` | `mode: image/text`、`targetId` 或 `targetIds`（≤100，多张只支持平均主色/语义）、文本、`kind`、阈值、`scope`、`limit` | token、范围、全部命中数、可翻页数、带分数的预览 |
| `images_get` | `ids≤50` | 详情、`[{id,name}]` 形状的标签、描述、能力标记；临时图片 `folderPath` 为「临时」并带 `temporary: true` |
| `image_sheet` | selector、`grid 1–5`、`resolution: thumb/detail` | 拼图图片、格子映射、`sheetId`、下一页 |

- `tags_list` 与 `folders_list` 都有 `total` 与 `nextCursor`：同级或标签超过 `limit` 时给出游标，原样传给 `cursor` 取下一页；单页上限仍是 500（`tags_list` 默认 100、`folders_list` 默认 200）。游标是 offset 语义的不透明字符串，解析失败报“游标无效”，不静默回到第一页。翻页不回快照：两次调用之间有写入时可能重复或漏掉条目，返回里的 `pagingNote` 已写明。
- `search_images` 始终返回 `total`；`pageSize` 超出 1–50 直接报错，不静默截断。分面计数按需开启，2 字搜索与超大集合较慢。开 `includeFacets` 时额外返回 `facetsNote`：标签分面不应用 `tagIds` / `tagPresence` / `excludeTagIds`，选中一个标签后仍能看到其它标签的计数；其余分面各自排除自身维度。搜索与集合视图的分面按命中集统计，那里的标签分面只含命中集内的标签。分面只给 `tagId` 与计数，名称用 `tags_list` 取。
- `find_similar` 的 `total` 是范围与阈值过滤后的全部命中数，`returned` 是可翻页/可全选的条数（受 `limit` 限制）。预览每行带 `score`：`scoreKind=distance`（phash/color）越小越近，`similarity`（embedding/pose/semantic）越大越近。结果复用应用唯一的相似结果槽：后一次相似或语义搜索会让前一个 token 失效。
- `find_similar` 的 `threshold` 一律是上限：pHash 是 0–256 距离上限（省略时默认 6），`color4` / `color8` 是 0–1 距离上限，`embedding` / `pose` 是 -1–1 的相似度下限；省略都不做阈值过滤。方向容易记错：`color` 传 0 会筛掉全部、传 1 等于不筛，`embedding` 传 1 会全部命中。多张参考图只支持 `color4`/`color8`/`embedding` 的平均，最多 100 张；`phash` 与 `pose` 只接受单张。
- `find_similar` 的 `scope` 与界面一致：`library`（默认）、`folder`（`folderId`、`includeSubfolders`，默认包含）、`collection`（`collectionId`）、`selection`（`imageIds`）、`view`（结构化 `query`）、`temp`（临时文件夹）。全库不解析 id；其它范围在 Rust 侧解析成 id 集合后与扫描取交集。全库扫描（`scope` 省略或 `library`）不包含临时图片；`scope=temp`、显式 id 或 `view=temp` 等显式范围可以命中临时图片。
- 语义搜索（`mode=text`）要求用英文短语：中文向量彼此挤在一起，实测无关句余弦 0.607 对英文 0.328，「用自己的描述找回自己」中文 1/3、英文 3/3。工具描述与界面占位文案都写明这一点；这一轮不做翻译层，也不换文本塔。
- 语义搜索连接不上 AI 服务时只返回“AI 服务未启动或无法连接”，不带内部地址与 OS 错误。
- `image_sheet` 的 `detail` 只允许 3×3 及以下，直接从原图解码；`thumb` 读缩略图包。输出格子标签始终按新拼图布局编号；以拼图格子为来源时，来源标签放在 `sourceLabel`（与输出标签相同时省略）。
- 每个格子带 `status`：`image` 有图、`missing` 有图但缩略图/原图读不出来、`empty` 这一格本来没有图；`thumbMissing` 只对 `missing` 为 true。被 `excludeCells` 排除的格子不占 `cells`，单独列在顶层 `excludedCells`。
- AI 要给用户留下一组结果时，服务端指令与 `collections_create` 描述都要求优先创建集合（例如「AI · 主题」），不要复制文件到图库外；复制会产生不参与去重/标签的副本，当前没有导出工具。
- `images_get` 的 `capabilities.embedding` / `pose` 读的是 `feature_blobs` 是否存在，意思是「算没算过向量」，不代表参与全库相似扫描：回收站与临时区的图片即使有向量也不会进入全库扫描结果。
- `library_info` 的 `tempFolder` 给出临时文件夹是否可用、内部路径、图片数与生命周期说明；普通查询默认不含临时图片，必须显式 `view: "temp"` 或使用显式 id。
- `library_info.features` 只统计普通图库（不含临时区）；临时区自己的特征数在 `tempEmbeddings` / `tempPoses`，AI 不应把它们当成整库覆盖度。`limits.maxTempFeatureImages` 是临时区单次生成的特征张数上限（100）。

### 写入（9）

| 工具 | 关键输入 | 返回 |
| --- | --- | --- |
| `images_update` | selector、评分 / 加标签 / 删标签 / 单图描述、`confirmedCount`、`dryRun?` | `affected`、`operationId` |
| `images_move` | selector、目标文件夹、`confirmedCount`、`dryRun?` | `taskId`；任务快照完成后带 `operationId` |
| `images_trash` | selector、`confirmedCount`、`dryRun?` | `affected`、`operationId` |
| `images_restore` | selector、`confirmedCount`、`dryRun?` | `affected`、`operationId` |
| `collections_create` | 名称、可选 selector、`confirmedCount`（带 selector 时）、`dryRun?` | 集合、`affected`、`operationId` |
| `collections_modify` | rename / delete、集合、`expectedItems?` | `operationId` |
| `collections_membership` | add / remove、集合、selector、`confirmedCount`、`dryRun?` | `affected`、`operationId` |
| `folders_modify` | create / rename / move / delete、`path` / `folderId`、`name`、`parentPath` 或 `parentId`、`dryRun?` | 文件夹信息、`operationId` |
| `tags_modify` | rename / delete / merge、`tagId`、`name` / `targetId`、`expectedImages?`、`dryRun?` | 标签信息、`affected` / `moved` / `duplicate`、`operationId` |

- `images_update` 一次只做一类修改（评分、加标签、删标签、单图描述之一），一次调用只产生一条历史记录。描述要求 selector 解析为 1 张图。
- 临时图片允许 `images_update`（评分、标签、描述），但不产生历史记录，结果里的 `operationId` 为 null；`images_move`、`images_trash` 与集合成员变更会同步拒绝临时图片（选择里有临时图就整批 `rejected`，不再先弹确认卡再让任务失败）。
- `images_move` 的 `dryRun` 把「已经在目标文件夹」算进 `skipped`（另给 `alreadyInTarget`），全是这种情况直接 `noop`；任务快照的 `completed` 是已处理张数，`skipped` 是其中没有移动的张数，`failed` 与 `failures` 来自同一份移动报告。
- `tags_modify` 三种动作都走操作记录、都能撤销：`rename` 只改名称，撞同名标签直接报错；`delete` 删除标签并从所有图片上移除它；`merge` 把 `tagId` 的图片全部并到 `targetId`（已存在的关系只去重），然后删除源标签行。`delete` / `merge` 可传 `expectedImages` 核对源标签当前图片数，不一致返回 `rejected`。三种动作不套 100 张阈值（范围是整个标签），询问档下 `delete` / `merge` 需确认、`rename` 直接执行。合并会删除源标签，撤销依赖操作记录。
- `folders_modify` 四种动作都走操作记录、都能撤销：`create` 用 `path`（缺失的上级一起建），`rename` 用 `folderId` + `name`，`move` 用 `folderId` 加 `parentPath`（空串表示根目录）或 `parentId`，`delete` 只删「无子文件夹、无图片、磁盘目录为空」的文件夹。临时文件夹与根目录不能改名、移动或删除。询问档下 `create` / `rename` 直接执行，`move` / `delete` 需用户确认。`images_move` 仍会为目标路径自动建目录，`folders_modify` 是显式入口。
- `collections_create` 带 selector 时在同一事务里建集合并加入图片，只产生一条记录；撤销会删除整个集合，重做按原 id 与条目顺序恢复。
- selector 可以解析到不存在、已在回收站或不在回收站的图片；出现这类 id 时整批拒绝并返回失败清单，不会静默少做。
- 产生操作记录时在标题前加 `AI · `。一次 MCP 写入最多 100 万张（query / ids）；相似结果最多 10,000 张；拼图最多 25 格。

### 记录（3）

| 工具 | 关键输入 | 返回 |
| --- | --- | --- |
| `history_list` | `limit?`、`kind?` | 记录、状态、`affected`、推导出的 `undoSteps` / `redoSteps`、`canUndo` / `canRedo`、阻塞原因、待释放统计；顶层 `canRun` 只在返回条目里真有可执行项时为 true |
| `history_undo` | `targetId`、`confirmedSteps`、`confirmedAffected`、`dryRun?` | `taskId` |
| `history_redo` | `targetId`、`confirmedSteps`、`confirmedAffected`、`dryRun?` | `taskId` |

- 撤销目标必须是 `applied`；撤销目标及之后所有 `applied` 记录，最新的先撤。重做目标是 `undone` / `redoing`，处理目标及下方连续的 `undone` 记录，最早的先做。
- 正式执行前必须先用 `dryRun` 拿到 `steps` 与 `affected`，执行时两者都要一致；执行前还会再核对一次，数量或状态变化就拒绝。
- 询问档永远确认；完全档不弹确认，但仍要数量核对。
- 撤销链包含 `optimize` 记录时，领域层返回“压缩操作不可撤销，原图已删除”；工具原样返回该错误，不自动换目标、不重试。
- 新写入会清空灰色重做分支；`history_list` 的 `note` 与 `history_redo` 描述都写明「下一次写入会删除这些可重做记录」，避免把「刚撤销的记录消失」当成丢数据。
- 撤销/重做是后台任务，可取消多条记录的执行；取消后已完成部分保留。

### 任务（2）

| 工具 | 关键输入 | 返回 |
| --- | --- | --- |
| `tasks_get` | `taskId?`、`waitMs≤30000`、`includeFailures?`、`includePairs?` | 任务快照；导入任务附带回执；不带 id 时返回活动任务、最近结束的任务与待确认列表 |
| `tasks_cancel` | `taskId` | — |

- `tasks_get` 传 `approvalId` 时等待用户批准/拒绝，并附带关联任务的状态；批准后启动的任务要用同一 `approvalId` 或任务 id 继续轮询。
- `tasks_cancel` 只允许取消 MCP 自己发起的任务；用户从界面启动的任务不能由 AI 取消。传 `approvalId` 等同于拒绝。
- 任务快照的可选 `operationId` 在写入记录产生后出现；进行中通常为空。
- 导入任务额外附带 `import` 回执：`target`、`imported`、`duplicates`（同值的 `duplicatesSkipped`）、`merged`、`tempReplaced`、`failed`、`similar`、`previewIds`、`startedAt`、`finishedAt`、`countsNote` 与失败清单。`countsNote` 一句话说明三个「重复」口径：`duplicates` 是按策略跳过的源文件数（merge 下为 0）、`duplicatePairCount` 是命中的库内重复对、`merged` 是被合并进已有图片的源文件数。回执不返回 `optimized` / `optimizedBytes`，MCP 不能控制压缩。`previewIds` 最多 25 张，可直接作为 `image_sheet` 的 `selector.ids`。
- 回执默认只给精确计数与截断标记：`duplicatePairCount`、`batchDuplicatePairCount`、`mergedImageIdsTruncated`、`importedPairsTruncated`、`similarPairsTruncated`、`failuresTruncated`。传 `includePairs=true` 才附带 `duplicatePairs`（`sourcePath` / `imageId` / `trashed`）、`batchDuplicatePairs`（`sourcePath` / `keptPath`）、`mergedImageIds`、`importedPairs`（`sourcePath` / `imageId`）与 `similarPairs`（`imageId` / `similarTo` / `distance`）。四组归因数组各自最多 200 条、独立截断；`similarPairs` 最多 100 条。
- `duplicatePairs` 是“源文件 ↔ 库内已有图片”；`trashed=true` 表示命中的那张图在回收站，需要时可 `images_restore`。`batchDuplicatePairs` 是同一次导入里内容相同的多份源文件，只保留 `keptPath` 那一份。`mergedImageIds` 只在使用 `duplicatePolicy=merge` 时出现，可用 `images_get` 核对标签并集与评分。`similarPairs` 是导入时算出的疑似重复：阈值固定 6/256、每张新图只给最近的一张、只覆盖新入库图片，被跳过的精确重复不参与。

### 导入（1）

| 工具 | 关键输入 | 返回 |
| --- | --- | --- |
| `import_start` | 本机绝对路径、`target: library/temp`、目标文件夹、`folderMode: flat/preserve`、标签、评分、`duplicatePolicy: skip/merge` | `taskId`、`pathCount`、`target`、`operationId`、`compressed: false` |

- 入队前先 `std::fs::metadata` 检查每个输入路径；有路径不存在或不可访问时直接 `rejected` + `missingPaths`，不上确认队列。
- 导入固定 `optimize = None`，不接受 `workerThreads`；即使请求里带了这两个字段也不透传。
- 单次最多 10000 个输入路径；扫描阶段最多处理 100000 个文件，超出时只处理前面的文件并记一条失败。
- `target=library`（默认）导入普通图库，完成后整次导入可以用 `history_undo` 撤销。`target=temp` 导入临时文件夹：固定平铺、只允许 `skip`、不写操作记录，`operationId` 为 null，关闭/切换图库或退出应用时清空；显式传 `merge` 或 `preserve` 会报错。
- 询问档只对 `target=library` 的导入要求确认；导入临时文件夹不需要确认，但仍写 `logs/mcp.log` 审计。只读档全部拒绝。
- 确认卡展开前 5 条来源路径、其余只给数量，并列出目标（图库文件夹或临时文件夹）、标签、评分与重复策略。
- 重复策略在导入前整批选定，导入前无法预知哪些源文件会撞库；`skip` 与 `merge` 都会在回执里给出精确重复对（`tasks_get(includePairs=true)`）。临时导入同样提供重复对，但只允许 `skip`。
- 临时区口径：图库导入的精确查重**不看临时区**。同内容图片已经在临时区时，写事务里先丢弃那一行（临时区是「只看不留」的暂存，关库即清空），再正常入库，因此「先导临时区看一眼、再决定入库」这条路是通的；被丢弃的行数记在回执 `tempReplaced`，临时目录里的原图副本在提交后清理。临时导入仍按原口径查重，命中图库或临时区的同内容图都算跳过，两类不分开计数。
- 导入不算 embedding 与描述：通过 MCP 导入的图片进不了语义搜索，工具描述要求 AI 说明这一点，需要时由用户在界面上跑批量 AI 任务。

### 临时区特征（1）

| 工具 | 关键输入 | 返回 |
| --- | --- | --- |
| `temp_features_generate` | `ids`（≤100）、`kind: embedding/pose` | `taskId`；完成后在 `tasks_get` 看逐张结果与失败清单 |

- 只接受临时文件夹里的图片 id；混入不存在、已删除或普通图库图片时整批拒绝（`status: "rejected"` + `invalidIds`），不做部分执行。
- 只做 `embedding` 与 `pose`，不生成描述；不写操作记录，不需要 `confirmedCount`。任务占 `ai` lane：正在排队的其它 AI/扫描任务会按 FIFO 等待，短写操作优先通过。
- 询问档每次确认；完全档直接执行；只读档拒绝。
- 临时区导入时已经算好 pHash 与主色，粗筛不需要生成；这个工具只补语义与姿势。库内没有对应特征时，生成临时特征也搜不出结果，工具描述要求先看 `library_info.features`。
- 特征写进 `feature_blobs`，临时区清空时随 `images` 行级联删除；feature pack 里的字节不会立刻回收，留待维护任务处理。

## 选择器

所有批量工具共享同一套 selector：

```json
{ "kind": "query", "query": { "...": "..." }, "excludeIds": [] }
{ "kind": "ids", "ids": [1, 2, 3] }
{ "kind": "sheet", "sheetId": "...", "cells": ["A1", "A2"], "excludeCells": ["A3"] }
{ "kind": "similarity", "token": "...", "offset": 0, "limit": 16 }
```

- `query`：结构化查询，字段与 `search_images` 相同（`view` 可用 `temp`）；写入时解析全部命中，不分页。查询临时图片后仍受写入限制：只能改评分/标签/描述。
- `ids`：明确 id，去重并保留输入顺序；不存在的 id 进入失败清单。
- `sheet`：引用 `image_sheet` 返回的格子；`cells` 省略表示整盘。
- `similarity`：引用 `find_similar` 返回的 token。`offset` / `limit` 在**读取与写入都生效**：写入按 `offset` 跳过前若干条、最多取 `limit` 条；`limit` 省略时取整个结果槽（最多 10,000 条）。
- `excludeIds` 在所有 selector 上生效；`sheet` 另有 `excludeCells`。

## 数量核对与写入语义

顺序是“查询 / 试算 → 看到数量 → 带数量再执行”：

- `dryRun` 返回 `selected`、`willChange`、`skipped`、`sample`，不写库。`willChange` 只算**真的会变**的图片：评分已经是目标值、标签已经存在、集合成员已经存在、已经在目标文件夹的都不计入；`skipped` 与 `willChange` 加起来等于 `selected`。标签操作的 `willChange` 是涉及的图片数，历史 `affected` 是新增/移除的关系条数。
- 目标状态与现状完全一致时直接返回 `status: "noop"` + `reason`，不写库、不产生 `affected=0` 的空记录。
- 正式写入必须带 `confirmedCount`，等于 selector 解析出的 id 数；缺失或不一致返回 `status: "rejected"` 与最新数量。
- selector 解析为空返回 `status: "noop"`，不产生历史记录。`collections_create` 只在带 selector 时套用这条；不带 selector 时按用户意图创建空集合。`collections_membership` 会先核对集合是否存在，不存在直接报错，不进确认队列。
- 部分 id 不存在、已在回收站或状态不符时返回 `status: "rejected"` 与失败清单，不执行任何一行。
- MCP 写入在本进程内串行；短写入立即占用 write lane，导入、移动、编辑、优化、撤销/重做或 AI/扫描任务占用时直接拒绝，不排队。
- 长任务（`library_import`、`library_move`、`temp_features_generate`）默认排队：调用立即返回 `taskId`，任务可能处于 `queued` 阶段；`tasks_cancel` 可取消排队中的任务。撤销/重做与清空记录不排队，被占用时直接失败。
- 一次工具调用对应一条历史记录；标签操作的历史 `affected` 是关系条数，工具结果同时给出 `selected`。

## 询问档确认队列

1. 工具返回 `{ status: "pendingApproval", approvalId, preview }`，不立即执行。
2. 应用右下角的非模态确认卡片与设置 → MCP 的待确认列表显示操作、范围、数量、示例与后果。
3. 用户批准后重新解析 selector 并核对 `confirmedCount`；数量或状态变化时确认单标记失败并返回新预览。
4. 拒绝、10 分钟未处理或关闭应用都会取消；`tasks_cancel(approvalId)` 等同于拒绝。
5. AI 用 `tasks_get(approvalId)` 等待结果；批准后启动的移动/撤销/重做任务会附带任务快照。

询问档的拦截条件：

- `import_start` 且 `target=library`（不论路径数；`target=temp` 不确认）；
- `images_trash`（不论张数）；
- `history_undo`、`history_redo`（不论条数）；
- `folders_modify` 的 `move` 与 `delete`（会成批改动图片路径或删掉目录；`create` / `rename` 直接执行）；
- `tags_modify` 的 `delete` 与 `merge`（会成批移除关系）；
- 任何解析出超过 100 张图片的批量写入（更新、移动、恢复、集合创建/成员变更）。

阈值固定为 100，不是设置项。

## 前端刷新

- 同步写入成功后后端发 `library-changed`，载荷包含 `source`、`tool`、`operationId`、`closeViewer` 与结构化 `change`（`kind`、`fields`、`tagAction`、`score`、`descriptionEmpty`）。
- `TaskCenterProvider` 把载荷转成统一的 `DataChange`，由 `LibraryView` 的 `planDataChange` 决定列表、选择、文件夹、计数、特征、分面与相似结果是否失效；外部写入拿不到受影响 id，元数据变化重取可见行，范围变化清空选择。
- 异步任务继续用 `task-progress` / `task-finished`；`task-finished` 同样转成 `DataChange`，同刻多个任务只拿到最后一条时退化为保守的完整刷新。
- 设置 → MCP 面板监听 `mcp-approval-changed` 与 `library-changed`，刷新待确认与最近 AI 操作。

## 审计日志

`logs/mcp.log` 记录：

- 每次工具调用的工具名、档位、结果状态、`selected` / `affected` / `operationId` / `taskId` 与耗时（`tags_modify` 的记录沿用同一格式）；阻塞线程异常终止也记一条 `status=error`。错误只写可读错误文本，不写返回值或整份参数。
- 导入来源单独一行：档位、目标文件夹、任务号、操作记录号、路径总数与前 20 个来源路径。总数写在路径之前，单行截断只会切掉路径尾部。
- 询问档的批准与执行结果；stdio 小桥的启动、连接错误与退出。
- 单行最长 600 字符，换行会被压成空格。设置页的“最近 AI 操作”仍从操作记录里筛选 `AI ·` 标题，不解析日志文件。

## 安全与限制

- 只绑定环回地址；缺少或错误的 Bearer 口令返回 HTTP 401。
- 应用未运行、MCP 服务未开启、图库未打开时，工具返回可读错误。
- 不提供永久删除、清空回收站、图片空间优化、像素编辑、批量 AI 生成的工具；唯一例外是 `temp_features_generate`（仅临时区、只做 embedding/pose、单次 ≤100 张）。
- 导入接受任意本机绝对路径，不做目录白名单；询问档必须确认，`full` 档会在设置页提示这一能力。导入固定不压缩，来源路径写入 `logs/mcp.log`。
- `readOnly` 档拒绝全部写入、导入与撤销/重做；`full` 档不弹确认，但仍有数量核对与上述能力边界。
- 不新建 headless 产品模式；便携包仍然只有一个 exe。
- 运行时文件是本地凭据，不要提交或分享；`.gitignore` 已排除 `mcp-runtime.json`。

## 代码落点

Rust：

- `src-tauri/src/mcp/runtime.rs`：服务启停、端口、口令、`mcp-runtime.json`、Bearer 校验、stateless Streamable HTTP。
- `src-tauri/src/mcp/bridge.rs`：`--mcp-stdio` 逐行代理，维护协商版本。
- `src-tauri/src/mcp/server.rs`：rmcp 工具适配器、工具描述与工具注册。
- `src-tauri/src/mcp/approval.rs`：待确认队列、状态与 10 分钟过期。
- `src-tauri/src/mcp/audit.rs`：`logs/mcp.log` 工具调用与导入来源审计。
- `src-tauri/src/mcp/selection.rs`：selector 解析（分页与全量 id）、排除与去重。
- `src-tauri/src/mcp/sheet.rs`：拼图映射缓存与缩略图/原图读取。
- `src-tauri/src/mcp/tools/read.rs`：只读工具、`find_similar` 的 `FindSimilarScope` 与多参考图参数。
- `src-tauri/src/mcp/tools/write.rs`：写工具、数量核对、询问判定与写后事件；`willChange` 预检（评分/标签/集合成员/目标文件夹）也在这里。
- `src-tauri/src/mcp/tools/folder.rs`：`folders_modify` 的四种动作、路径解析与删除前置检查。
- `src-tauri/src/mcp/tools/tag.rs`：`tags_modify` 的重命名/删除/合并、`expectedImages` 核对、询问判定与确认预览。
- `src-tauri/src/mcp/tools/history.rs`：记录列表、撤销/重做预览与批准执行。
- `src-tauri/src/mcp/tools/task.rs`：任务与确认单查询、等待、取消与导入回执（含 `includePairs` 归因数组）。
- `src-tauri/src/mcp/tools/import.rs`：导入输入校验、固定不压缩、确认预览与任务启动。
- `src-tauri/src/mcp/tools/feature.rs`：`temp_features_generate` 的临时区校验、100 张上限、确认预览与任务启动。
- `src-tauri/src/library/history.rs`：`write_with_id`、`prefix_label`、`preview_undo` / `preview_redo`。
- `src-tauri/src/library/metadata.rs`、`collections.rs`：带 `operationId` 的写入口、`merge_tags_with_id` / `merge_preview` 与 `create_with_images`。
- `src-tauri/src/app/state.rs`：确认队列、MCP 写入串行、MCP 任务记录、导入回执缓存、`library-changed`、批准执行入口；`start_mcp_import` 固定清空压缩与线程参数并加 `AI ·` 前缀；`start_temp_features` 走 write lane。
- `src-tauri/src/app/commands.rs`：`list_mcp_approvals`、`approve_mcp_approval`、`reject_mcp_approval`。
- `src-tauri/src/import/pipeline.rs`、`import/job.rs`：重复对、批内重复、合并目标与源文件映射的采集、上限与截断标记。
- `src-tauri/src/imageops/contact_sheet.rs`：拼图布局、编号与 JPEG 编码。
- `src-tauri/src/library/folders.rs`：文件夹建/改名/移动/删除与撤销记录；`info` / `find_id` / `deletion_blocker` 供 MCP 预检复用。
- `src-tauri/src/library/import.rs`：`find_by_hash_state` 的临时区口径、`discard_temporary_duplicate`、pHash 索引排除临时区。
- `src-tauri/src/library/similarity.rs`、`ai/embedding.rs`、`ai/pose.rs`：相似扫描返回截断前的命中总数（`*_with_total`），接受范围 id 集合、排除集合与阈值；全库扫描排除临时图片，显式范围仍可命中；主色平均在 `imageops/colors.rs::aggregate_palettes`，embedding 平均在 `ai/embedding.rs::target_average`。
- `src-tauri/src/ai/runtime.rs`：相似结果槽改存 `(id, 分数)`，`similarity_scores` 供预览带 `score`；`AiError.transport` 让语义搜索连接失败只报可读文案；`start_temp_features` 复用批量 AI 任务并失效语义结果与均值缓存。

前端：

- `src/app/types.ts`、`src/app/api.ts`：MCP 确认单类型与三个命令；`AppInfo.exePath` 供客户端配置使用。
- `src/app/McpPanel.tsx`：设置 → MCP 的三档、待确认与最近 AI 操作。
- `src/app/McpApprovalCard.tsx`：确认卡片（导入额外显示来源路径、标签/评分与重复策略）、全局非模态宿主、`useApprovalActions` 与 `mcp-approval-changed` 监听。
- `src/app/TaskCenterProvider.tsx`：挂载确认卡片宿主，把 `library-changed` 与 `task-finished` 转成 `DataChange`。
- `src/app/LibraryWorkspace.tsx`：接收任务/MCP 变化，交给自己发起的拖放与导入变化一起排队，再传给 `LibraryView`；`closeViewer` 为真时关闭查看器。
- `src/app/taskTypes.ts`：任务快照的 `operationId` 与 `tempFeatures` 任务类型。

## 验证

自动检查：设置 v5→v6 迁移与端口校验、Bearer 校验、stateless HTTP initialize/tools/list、selector 全量解析、搜索总数与分页与 `pageSize` 上限、拼图布局/缺图/缓存/来源标签、相似 token 失效与命中总数/分数、`find_similar` 的 scope 收窄与多参考图平均/拒绝、全库扫描排除临时图而显式范围可命中、`phash_groups` 跳过临时行、0 张标签过滤、切档保留端口与口令、`write_with_id`、描述写入的改动数与空操作、撤销/重做预览步数与影响数、压缩记录阻断、确认数量与三档判定、`library_info` 三档的 `writeAllowed`/`approval.threshold`、`tags_list` 游标遍历与非法游标、`folders_list` 分页、标签缺失/排除筛选、`tags_modify` 的权限划分与 `expectedImages` 核对、标签合并的迁移与撤销重做、工具注册数量（24）、临时图库只读工具、导入输入与路径校验、`optimize` / `workerThreads` 不进入 MCP 导入、导入预览路径截断、导入回执字段与隐藏压缩字段、精确重复/批内重复/合并目标/源文件映射与 `includePairs` 默认省略、失败计数取精确值、审计单行截断、导入缩略图预览上限 25、临时导入 target 校验（清空文件夹、拒绝 merge/preserve）、临时视图查询与元数据不写历史、`library_info` 临时计数/路径/临时特征计数、`temp_features_generate` 的临时 id 校验与 100 张上限、similarity 选择器写入按 offset/limit 切片、`willChange` 只算真实改动并在全无变化时 noop、集合存在性与成员预检、移动的 `alreadyInTarget` 与任务 skipped/failed 对齐、临时图在移动/移入回收站/加入集合前被同步拒绝、导入路径预检、临时区同内容行在图库导入时被丢弃、`find_by_hash_state` 的临时区口径、`history_list` 顶层 canRun 推导、拼图 `status` 与 `excludedCells`、文件夹 create/rename/move/delete 的领域变体与删除前置检查。

真实客户端验收需要用户执行：

1. 启动应用并打开图库，在设置 → MCP 开启服务，确认状态为“服务运行中”。
2. 在设置 → MCP 点“复制客户端配置”，粘贴到支持 stdio 的客户端（命令是当前进程路径，参数 `--mcp-stdio`），调用 `library_info`，确认档位与限制。
3. 只读档调用 `images_update`，确认得到“当前是只读模式”。
4. 询问档 `dryRun` 一个搜索/拼图 selector，拿到 `selected` 后带 `confirmedCount` 执行评分或加标签，确认设置页“最近 AI 操作”出现 `AI ·` 记录且网格刷新。
5. 询问档移入回收站，确认右下角出现确认卡片；批准后图片进入回收站、查看器关闭；拒绝或等待 10 分钟确认操作取消。
6. 对一条记录 `history_list` → `history_undo` dryRun → 带 steps/affected 执行，确认任务中心进度与撤销结果；再重做。压缩记录应返回“压缩操作不可撤销”。
7. 启动一次导入/优化后调用短写工具，确认得到明确冲突错误且没有被排队；再启动第二个长任务，确认它进入 `queued` 并可用 `tasks_cancel` 取消。
8. 关闭 MCP 服务，确认客户端收到明确连接错误；重启应用后确认 `mcp-runtime.json` 已更新。
9. 询问档调用 `import_start` 导入几张图片，确认右下角与设置页出现确认卡，卡片列出来源路径、目标文件夹、标签/评分与重复策略；批准后任务开始，拒绝或超时则不导入。
10. `tasks_get(taskId)` 等到导入完成，确认附带回执的 `imported` / `duplicates` / `merged` / `previewIds` 与失败清单；用 `image_sheet(selector.ids=previewIds)` 看到刚入库的图。
11. 对这次导入 `history_list` → `history_undo`，确认整次导入被撤销；确认 `logs/mcp.log` 有工具调用与导入来源行，且没有压缩字段或压缩行为。
12. 从便携目录启动 exe（含 `server/`，不含安装器），重复第 2 步，确认 `--mcp-stdio` 指向当前 exe 且能正常调用 `library_info` 和 `import_start`。
13. `find_similar` 分别传 `scope: {"type":"folder","folderId":…,"includeSubfolders":false}` 与 `targetIds`（两张图，`kind: color8`），确认结果只来自范围、参考图全部被排除，`detail.referenceCount=2`、`scope` 文案与界面一致。
14. `import_start` 传 `target: "temp"` 导入几张图：询问档应直接开始、不出现确认卡；`tasks_get` 回执 `target=temp`、`operationId=null`；`search_images` 用 `view: "temp"` 能看到图片，普通 `view: "all"` 看不到。
15. 对 `view: "temp"` 的图片调用 `images_update` 改评分，`history_list` 不应新增记录；调用 `images_trash` 或被移出临时视图范围应得到明确拒绝。
16. 把 3 张已在库内的图（其中 1 张在回收站）重新导入一次，`tasks_get(includePairs=true)` 返回 3 条 `duplicatePairs`，`imageId` 指向首次入库的图，回收站那张 `trashed=true`；不传 `includePairs` 时回执里没有这几组数组。
17. 对同样 3 张改用 `duplicatePolicy=merge` 重跑，`mergedImageIds` 与上一步的 3 个 id 一致，`images_get` 能看到标签并集与较高评分。
18. 同一目录放 2 份完全相同的图导入，`batchDuplicatePairs` 给出一对 `keptPath` / `sourcePath`，`duplicates=1`；`target=temp` 导入同样返回重复对且不写操作记录。
19. 临时区导入 5 张图后调用 `temp_features_generate(kind=embedding)`：询问档出现确认卡，批准后任务用 write lane 运行；完成后 `library_info.features.embeddings` 不变、`tempEmbeddings=5`。
20. 以临时图为参考、`scope=temp` 搜语义能命中临时区内的图；改成 `scope=library` 后结果里不出现任何临时图。清空临时区后再搜语义不报错。
21. `ids` 里混入一张普通库图片，`temp_features_generate` 整批拒绝并列出 `invalidIds`；生成中途取消后已完成张数保留、任务为 `cancelled`。
22. `tags_list` 不带 `limit`：返回 100 条 + `total` + `nextCursor`；把 `nextCursor` 传给 `cursor` 再调一次，拿到剩余标签且没有重复 id；`nextCursor` 为 `null` 表示翻完。
23. `search_images` 传 `filters.tagPresence: "missing"` 只能拿到没有标签的图片；再加 `excludeTagIds`，结果里不再出现带该标签的图片。
24. 切到完全档调 `library_info`：`approval.threshold` 为 `null`、`appliesToCurrentMode` 为 false；问 AI「完全档批量写入需不需要确认」，应答不需要。
25. `tags_modify` 合并两个近义标签：先 `dryRun` 看 `moved` / `duplicate`，正式执行后源标签消失、目标标签图片数为两者之和；`history_list(kind: "tagMerge")` 有 `AI ·` 记录，撤销后两个标签与关系都回来。

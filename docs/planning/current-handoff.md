# 当前阶段状态

日期：2026-10-25
阶段：**图片空间优化批次 1（只读内核 + 估算）已完成（2026-10-25）**，批次 2（写路径 + 两步流程）未开始。操作撤销批次 1–3（含重做增补）已完成并归档，待用户真实窗口验收。此前 P0–P4 体验整改、S4 查看器、S6 AI 客户端、S7 相似度检索、单库与多级文件夹（A–C）、旧库全量迁移（D）均已落地，主库 390 542 张、独立测试库 9 455 张已迁移并自动验证。P0、P2、P4 已通过用户真实窗口手工验收；P1 的界面观感与 P3 的整体界面仍待验收。下一阶段按 [路线图](roadmap.md) 继续。

历史批次记录已归档，本文件只保留当前状态、基线、待验收与约束。AI 服务端与便携化已实现，待用户实测。

## 图片空间优化批次 1（2026-10-25）

- `imageops/optimize.rs`：三档质量与两套 PSNR 底线、bpp / 已优化 / alpha / 16 位判据、阶梯编码（75→82→85→90→95→97）、PSNR 与局部坏点一次遍历、长边缩放、JPEG 源 EXIF（删 Orientation）与 ICC 搬运。
- `library/optimize_estimate.rs`：有损/无损 × 6 个 bpp 桶分层，按候选体积分配样本（每层至少 3 张），真实解码与编码，分层外推、300 次 bootstrap 误差、耗时、质量分布、分文件夹明细；全程只读，无临时文件。
- 任务：`TaskKind::OptimizeEstimate`（scan lane）、`BatchTaskKind::OptimizeEstimate`；`start_batch_task` 携带 `optimize` 与 `estimateSampleSize`，结果由 `get_optimize_estimate` 读取，内存保留最近 5 条。
- 前端：工具栏「任务 → 优化图片空间…」打开 `src/app/OptimizeDialog.tsx`，可配置范围、档位、输出格式、长边、重算开关、跳过已处理与抽样数量，并显示进度。
- 结果区：处理/跳过按张数的环图 + 图例（含体积与跳过分类）、预计节省与耗时卡片、收益折线图（右侧列 20% / 50% / 100% 里程碑的累计节省与耗时）、质量分布、分文件夹明细与样本失败清单；图表在 `src/app/OptimizeCharts.tsx`，不引第三方图表库。
- 为此 `CurvePoint` 增加 `elapsedSeconds`（按像素折算的累计耗时），`OptimizeEstimate` 增加 `failedBytes`（失败样本外推体积），使图例里各分类体积之和等于“跳过”总体积。
- 未做：schema v6 与 `optimized_at`（当前「已处理过」计数恒为 0）、执行任务、不可撤销记录、估算凭证；这些属于批次 2。
- 自动检查：`cargo test --lib`（196 通过、5 ignored）、`npx tsc --noEmit`、`cargo fmt --check`、`cargo check`、`npm run build`、`git diff --check`（结果见本轮最终报告）。

## AI 服务端与便携化（待验收）

- `server/` 实现 `/v1`：health、models/ensure、embed/image、embed/text、pose、shutdown；描述本轮不实现。
- SigLIP2 图片塔/文本塔从同一份 safetensors 按前缀独立加载，不实例化另一塔；同一时间只驻留一个，空闲 300 秒卸载（`--idle-unload-seconds`，0 关闭）。CUDA 用 fp16，CPU 用 fp32。语义搜索只加载文本塔，图片模型名与维度从 health 读取。
- MediaPipe Pose 使用 `python_embeded` 内自带的模型，33 点顺序与原项目一致；无人时返回空数组。
- 设置 v4：新增 `pythonPath`、`serverScript`、`modelPath`、`clipModelPath`，删除 `executablePath`；相对路径按程序根目录解析；填写本机地址、Python 与脚本后随应用启动；保存启动配置会重启托管服务。
- 设置、主日志与 `ai-server.log` 移到程序目录；不读旧 AppData 设置、不做迁移；程序目录不可写时启动失败。
- 移除 NSIS；`npm run pack:portable` 产出 exe + `server/`。`python_embeded/` 与 `resources/models/` 由用户放入，不打包、不下载。
- 模型由首次使用自动加载，设置页与批量任务对话框只显示加载状态；搜索框合并匹配/语义两种模式。
- 待用户实测：真实模型两塔加载与切换、ensure、批量 embedding、pose、语义搜索、空闲卸载、便携目录运行与日志落点。

## 基线

- 图库：主库 `G:\Lilith Image Library`（390 542 张、38 个文件夹、81 649 条 pose、9 个缩略图段），独立测试库 `G:\Lilith Test Library`（9 455 张）；失败内容在 `G:\Lilith Image Library Failed`（247 条超过 1 亿像素）。
- 存储版本：schema v5（v1→v2→v3→v4→v5 链式迁移）、设置 v5（v1→v2→v3→v4→v5）；设置与日志位于程序目录，不读旧 AppData 设置。
- 20 万行元数据基准（`cargo test --lib -- --ignored --nocapture perf_metadata_200k`）：单级排序首页 0.66-0.76 ms（文件夹 42 ms，走 id 列表）、6 级排序首次建 id 列表 213 ms、后续翻页 0.83 ms、offset 199 850 的深页 0.83 ms、schema v3→v4 建两个索引 459 ms。全部数字见 [性能基准](../guides/performance.md)。
- 排序指标 7 个（导入时间 / 修改时间 / 评分 / 文件大小 / 像素尺寸 / 文件夹 / 集合内顺序），最多 7 级；单级且非文件夹走键集游标，其余走进程内有序 id 列表。

## 操作撤销批次 1（2026-10-25）

- schema v5：`operations` / `operation_chunks` / `undo_runs`，`images.purge_operation_id` / `origin_operation_id` 与待释放索引；v4→v5 只新增结构，不改现有行。
- 设置 v5：`history.maxSteps`（默认 100，范围 10–500）；保存后立即裁剪当前图库，被裁掉的删除记录安排释放。
- `library/history.rs`：同步 `write`、live operation、列表/裁剪/启动恢复、撤销引擎与释放引擎；批次 1 逆操作覆盖移入回收站、恢复、永久删除、清空回收站。
- 永久删除/清空回收站改为只写待释放标记；行与文件在记录过期或清空记录后由 `ReleaseGarbage` 删除。普通视图、回收站、全库搜索与 `rows_by_ids` 都排除待释放行。
- `TaskKind::Undo` / `ReleaseGarbage` 走 write lane；单条记录撤销与释放不可取消，多条记录撤销只在记录之间检查取消；撤销在 write lane 忙时直接冲突。
- 前端：工具栏“记录”按钮与 `OperationHistoryPanel`，支持线性撤销、失败摘要、待释放统计与清空记录；撤销完成后关闭查看器并刷新网格/文件夹/计数。确认文案改为“延迟释放”，新增撤销/清空记录确认。
- 自动检查：`npm run typecheck`、`npm run build`、`cargo fmt --check`、`cargo check`、`cargo test --lib`（168 通过、4 ignored）、`npm run test:web`（21 通过）、`git diff --check` 全部通过；`perf_metadata_200k` 复测见 [性能基准](../guides/performance.md)。
- 未做：真实窗口手工验收；批次 3 已在后续完成。

## 操作撤销批次 2（2026-10-25）

- 编辑保存接入 `history::write`：旧原图硬链接（失败复制）到 `.artbank/undo/<op>/`，事务内写编辑前快照；提交后才删旧原图。撤销恢复内容列、重新编码缩略图并递增 `thumb_version`、清空 `feature_blobs`、删除新原图与预览缓存。
- 图片移动接入 `history::write`，记录 `moves`；撤销按当前文件夹幂等回移，失败回滚本记录已完成的文件移动。
- 文件夹创建/重命名/移动/删除/排序全部记录历史；逆操作复用同一套目录与子树路径逻辑，创建只删仍为空的新建项，删除用原 id 重建。
- 导入使用 live operation：新增行写 `origin_operation_id`，每批事务追加新建文件夹/标签与合并快照，收尾 `finish_live` 后裁剪；撤销按 origin 分块删行删文件、恢复合并项、清理本次新建且已为空/无引用的文件夹与标签。冲突或线程启动失败会删除空记录。
- 启动恢复：`pending` 导入按 origin 行或已有 chunk 转 `applied`，否则删除；`undoing` 记录从最小 id 续做，所有逆操作可重复执行。
- 自动检查：`cargo fmt --check`、`cargo check`、`cargo test --lib`（173 通过、4 ignored）全部通过。前端与 schema/设置无改动。
- 临时限制（批次 3 已解除）：元数据/标签/集合写路径在批次 3 前不进入时间线。
- 未做：真实窗口手工验收；批次 3 见下。

## 撤销后重做增补（2026-10-25）

- 撤销不再删除记录：成功记录标为 `undone`、写入 `undone_at`，列表里变灰；“当前状态”分隔线标出当前撤销位置。
- 撤销时把撤销前状态写成 `format='redo'` chunks，原始逆数据不动；编辑的新原图与导入的原图改为移入 `.artbank/undo/<op>/`，重做时移回。
- 新增 `TaskKind::Redo`、`AppState::redo_to`、`redo_to_operation` 与 `run_redo`。重做目标是“重做到这里”，处理目标及下方连续的 `undone` 记录；开始前整批标为 `redoing`，崩溃后按最大 `redoing` id 续做，不会越过用户选择的目标。
- `history::write` 在同一事务里删除 `undone` / `redoing` 记录；新的图库操作清空灰色分支，暂存目录由 `cleanup_undo_dirs` 清理。导入在确实写入内容的批次里调用 `discard_redo_in_tx`（详见下方整理记录）。
- 前端历史面板：灰色记录、当前状态分隔线、动态“撤销到这条之前 / 重做到这条”、重做失败摘要与重试；查看器在撤销/重做后都关闭。
- 自动检查：`cargo fmt --check`、`cargo check`、`cargo test --lib`（176 通过、4 ignored）通过；前端检查见本轮最终结果。
- 未做：真实窗口手工验收；批次 3 见下。

## 操作撤销批次 3（2026-10-25）

- 评分、描述、标签设置/增删/改名/删除改为 `history::write`：评分记 `triples`，描述与标签关系记 JSON；撤销恢复旧值/旧关系，删除的标签用原 id 重建，重做从 `redo` chunks 恢复。
- 批量 AI 描述使用 live 操作：`begin_live` 建 `pending` 记录，每张写入在同一事务里追加旧描述与 `affected`，确实写入时结束重做分支；取消/失败后已写描述仍可撤销。单张 `ai_describe_image` 走普通记录，命令结束调用 `after_history_write` 裁剪。
- 集合创建/重命名/删除/排序/加入/移出/运算全部接入记录；创建、删除、运算与相似分组保存完整快照（行 + 条目位置），撤销删除、重做按原 id 重建。
- 新增 `create_collections_from_groups` 命令：相似分组保存改为一次事务创建全部集合，只产生一条记录；前端 `SimilarityGroupsDialog` 不再循环调用建集合/加图片。
- 历史面板的“项/张”单位把集合结构与标签改名按项计；其余按图片计。
- 基准：`perf_history_200k` 实测历史列表 0.88 ms、裁剪 1.64 ms、20 万行待释放统计 26.20 ms、标记 2.84 s、数据库删行 26.77 s；数字与说明见 [性能基准](../guides/performance.md)。
- 自动检查：`npm run typecheck`、`npm run build`、`npm run test:web`、`cargo fmt --check`、`cargo check`、`cargo test --lib`（182 通过、5 ignored）、`git diff --check`（结果见本轮最终报告）。
- 未做：真实窗口手工验收；真实磁盘上 20 万行释放任务的文件删除耗时。

## 批次 3 收尾与归档（2026-10-25）

代码整理（不改行为契约）：

- `library/history.rs`：`collection_snapshot` 升为 `pub(crate)` 并被 `collections` 复用，删掉 `collections.rs` 里逐字重复的 `snapshot`；集合条目的分批删除收口成 `delete_collection_items`（撤销“加入”与重做“移出”共用）；`redo_tags` 的五个重复遍历合并成一次按 seq 顺序执行的 `match`；`redo_visibility` 的 `match { _ => {} }` 改成 `if let`；去掉 `undo_tags` 里多余的 `sort_unstable`（SQL 已按 `tag_id` 排序），同名合并判断简化为 `target_id != tag_id`。
- `library/metadata.rs`：`write_ai_description` 返回 `Option<DescriptionRecord>`，去掉恒为 `true` 的第二个返回值；`remove_tags` 去掉“先查存在关系、再按 400 分批删”的多余分块——查出来的存在关系本来就不超过一个 SQL 批次。
- `ai/runtime.rs`：只有描述任务的收尾才裁剪历史并清理暂存目录；embedding/pose 任务不再每次跑一遍 `trim`。
- `SimilarityGroupsDialog`：空 id 的提前返回移到 `setSaving` 之前。
- 文档纠正：`library.md` 写明 `affected=0` 的空记录回滚时也不清空重做分支（没有实际改动不算新时间线）；`roadmap.md` 补上三批操作撤销的插入说明（此前路线图没有这条）。
- 归档：计划与实现记录移入 `archive/`，`archive/README.md` 与本文归档表同步；`docs/planning/` 下不再留批次文档。
- 自动检查：`npm run typecheck`、`npm run build`、`npm run test:web`（21 通过）、`cargo fmt --check`、`cargo check`、`cargo test --lib`（182 通过、5 ignored）、`git diff --check` 全部通过。

## 批次 2 整理与修正（2026-10-25）

代码整理（不改行为契约）：

- `library/history.rs`：删掉为后续批次预留但无人调用的死代码（`OperationDraft::add_affected` / `ids`、`append_live_ids` / `append_live_json` / `add_live_affected`、`RecoveryPlan::is_empty`、`remove_undo_dir`、`decode_ids`），去掉模块级 `#![allow(dead_code)]`；长任务统一走 `append_live_in_tx` / `add_live_affected_in_tx`。`RecoveryPlan.resume_redo` 的注释原来写成“最小 id”，实际取最大（用户选中的目标）。
- `app/state.rs`：`after_background_write` / `after_file_write` / `after_settings_changed` 三个同义别名合并成 `pub(crate) after_history_write`，注释列明四个触发点。
- `HistoryPage.canUndo` 改名 `canRun`：这个门禁同时管撤销与重做，旧名会让人以为重做另有条件。
- `library/folders.rs` 删掉错位在 `folder_id` 上的撤销说明；`library/metadata.rs` 的 `crate::library::history::FolderRef` 改用已导入的 `history`；`OperationHistoryPanel` 去掉冗余条件与重复三元、宽度只由 CSS 决定、单位判断合并成一个函数。

修正：

- **导入被 write lane 冲突拒绝时会清掉灰色记录**：`begin_live` 原来在同一个事务里删除 `undone` / `redoing`，而导入是先建记录再 `tasks.launch`——冲突被拒绝时什么都没写，用户可重做的记录却已经没了。现在 `begin_live` 只建 `pending` 记录，由确实写入内容（新增/合并/新建文件夹或标签）的导入批次调用新的 `discard_redo_in_tx`。回归测试 `pending_import_without_content_keeps_redo_branch`。
- `redo_move` 原来要求 `moves` 非空：只新建了目标文件夹、没有任何图片移动成功的移动记录无法重做。现在允许只有文件夹行的记录。
- 文件夹撤销/重做（`undo_create` / `undo_path_change` / `undo_delete` / `undo_reorder` 与对应的 `redo_*`）此前没有任何测试，文档却写成已覆盖。新增 `undoes_and_redoes_folder_records_in_linear_order`，串起创建 → 重命名 → 移动 → 同级排序 → 删除的整条撤销与重做。
- 自动检查：`npm run typecheck`、`npm run build`、`cargo fmt --check`、`cargo check`、`cargo test --lib`、`npm run test:web`、`git diff --check`（结果见提交信息）。

## 批次 1 收尾（2026-10-25）

代码整理（不改行为契约）：

- `library/schema.rs`：操作记录三张表与待释放索引抽成 `HISTORY_SCHEMA_SQL`，新建库与 v4→v5 迁移共用一份 DDL，避免两处漂移。
- `library/metadata.rs`：`delete_images` / `empty_trash` 去掉已经用不到的 `root` 参数；取待删除行的分支拆成 `purge_targets`；`set_trashed` 把两组 `deleted_at IS [NOT] NULL` 与赋值表达式提到循环外，去掉 SQL 里内联的 `if` 拼接。
- `app/state.rs`：`undo_to` 的三次读取合并成一次读事务（`OptionalExtension` 取代吞掉错误的 `.ok()`）；`after_history_write` / `schedule_release` 直接解构 `library_handles`；新增 `after_library_open`（先续做中断的撤销，再按上限裁剪），打开图库时也裁剪一次，与文档写的一致。
- `library/history.rs`：去掉与模块级 `#![allow(dead_code)]` 重复的逐项允许；新增 `HistoryKind::releases_files` 收口“记录被裁掉要释放文件”的判断；不支持撤销的类型改成用户可读的错误，类型名只进日志。
- `app/tasks.rs`：删掉只在测试里用的 `register_for_test`，测试直接调用私有的 `register`。
- 前端：`formatBytes` 收口到 `src/shared/format.ts`（图库、查看器、确认文案与记录面板共用一份实现）；记录面板的冲突提示按“撤销 / 清空记录”分别给标题，进度行不再重复“正在撤销”前缀；延迟释放提示只在真的移出了图片时出现。

性能与文档：

- 修掉一处查询回退：分页查询无条件加 `purge_operation_id IS NULL` 会让 20 万行按文件夹排序的首页从 39 ms 涨到 81 ms（该列不在排序 id 列表的覆盖索引里）。现在只在回收站视图与全库搜索加，普通/集合视图靠 `deleted_at IS NULL` 挡住；A/B 数字与复测结果写进 [性能基准](../guides/performance.md)。
- 文档纠正：`overview.md` 的设置/schema 版本、`.artbank/undo/` 目录、原图真正删除的时机、分层职责与批次记录；`AGENTS.md` 补操作记录路由与“写路径必须接入记录”的决策；`library.md` 补待释放行的排除方式与打开图库时的裁剪。
- 原“本阶段收尾”里属于 P1–P4 的整理记录移入 [体验整改 P1–P4 实现记录](archive/2026-10-25-体验整改P1-P4记录.md)。

## 归档

| 文档 | 内容 |
| --- | --- |
| [图库操作撤销实现记录](archive/2026-10-25-操作撤销实现记录.md) | 批次 1–3 的操作记录、撤销与重做实现、关键取舍、自动检查与性能基准。 |
| [图库操作撤销计划](archive/2026-10-25-操作撤销计划.md) | 数据模型、逐操作逆数据、三批划分、测试矩阵与验收清单；三批均已完成。 |
| [迁移后体验整改计划](archive/2026-10-24-体验整改计划.md) | 17 项体验问题的定位、P0–P4 批次划分、决策记录 |
| [体验整改 P0 基础层记录](archive/2026-10-24-体验整改P0记录.md) | 主题变量、设置 v3、schema v3、公共控件、回收站确认 |
| [体验整改 P1–P4 实现记录](archive/2026-10-25-体验整改P1-P4记录.md) | 浏览主界面、任务总线、查看器与编辑、文件夹与排序 |
| [S4–S7 与单库迁移记录](archive/2026-10-25-S4-S7与单库迁移记录.md) | 查看器与几何编辑、AI 客户端与语义搜索、单库与多级文件夹、旧库迁移、相似度检索 |
| [S0–S3 实现记录](archive/2026-10-23-S0-S3-实现记录.md) | 工程骨架、导入管线、虚拟网格与查询、元数据/搜索/集合 |

## 待验收

- **操作撤销批次 1（真实窗口）**：工具栏“记录”面板滚动与位置；移入回收站 → 清空回收站 → 撤销，图片回到回收站；清空记录后文件真正释放；批量操作只出现一条记录；任务运行时“记录”按钮置灰；撤销后网格/文件夹计数/查看器状态正确。
- **操作撤销批次 2（真实窗口）**：导入（含重复合并、取消后已完成部分）→ 撤销，图片、原图文件、本次新建的空文件夹与标签正确回退；编辑保存 → 撤销，原图、缩略图、`feature_blobs` 与预览缓存一致；批量移动 → 撤销，文件回到原文件夹；文件夹创建/重命名/移动/删除/排序逐项撤销；撤销到一半关闭应用，重开后能继续且不留下可见孤儿文件；线性组合（导入 → 编辑 → 移动 → 回收站 → 清空）逐层撤销结果正确。
- **撤销后重做（真实窗口）**：撤销几条后记录变灰、“当前状态”位置正确；选中灰色记录重做到指定位置，更新记录仍保持灰色；编辑与导入重做后原图/缩略图/标签/文件夹恢复；重做到一半关闭应用，重开后只续做本次选中的记录；任意新操作后灰色记录消失、暂存文件释放；重做失败提示与再次重试。
- **操作撤销批次 3（真实窗口）**：评分/描述/标签设置/批量加删标签/标签改名/删除逐项撤销与重做；集合创建/改名/删除/排序/加入/移出/并集交集差集撤销与重做，条目顺序与 `updated_at` 正确；相似分组“每组一个集合”和“合并保存”各只出现一条记录，撤销整批消失、重做整批恢复；批量 AI 描述（服务可用时）取消后已写描述可撤销；历史面板中集合结构记录显示“项”、图片记录显示“张”。
- **主库真实窗口**：应用打开 `G:\Lilith Image Library`（设置已指向该库），首次打开会把 schema v4 升到 v5（只新增表、列与索引）。检查树/计数/缩略图/查看器/pose/搜索/评分/标签/描述/编辑/AI/S7。
- **独立测试库**：从设置页切到 `G:\Lilith Test Library`，确认 9 455 张、6 个文件夹、87 pose 与缩略图正常。
- **体验整改 P1 界面**：1040×640 与 1440 宽度下两行工具栏不换行、面包屑渐隐、缩略图裁剪/适应、详情侧栏开关不丢选中项、右键菜单分组与键盘导航、新建集合预填名称、筛选零值隐藏、设置分类切换时对话框尺寸不变。
- **体验整改 P3 界面**：先裁剪再旋转/翻转时选区跟随、撤销/重做（标签、50 步、滑块合并）、只改 JPEG 质量能保存且体积变小、体积预估误差（抽样 20 张）、F11 与系统快捷键进出全屏、胶片带滚动/加载/开关持久化、姿势骨架低缩放清晰度、标签筛选后停在原图。
- **单库外壳与文件夹**：欢迎页创建/打开、启动自动打开、设置页换库、关闭图库；文件夹展开/多选/面包屑/右键菜单/空文件夹；子树筛选与计数；导入“平铺 / 保留源目录”与目标文件夹选择器；文件夹操作在目录被占用、无权限、目标已存在时的失败与回滚。
- **导入**：文件/目录选择、拖放、进度、取消、结果预览与失败详情；导入期间关库被拒绝、取消后已完成项保留。
- **浏览与检索**：网格/列表滚动、150 张缩略图从请求到可见、列布局、20 万行帧率与内存；搜索（当前视图/全库）、扩展筛选、筛选计数加载态、键盘与 Shift/Ctrl 选择、Ctrl+A、右键菜单。
- **详情/批量编辑与集合**：评分、标签自动补全、描述、文件夹移动、复制哈希/路径；标签重命名/删除影响提示；集合 CRUD/排序/运算/加入移出。
- **回收站**：移入/恢复/永久删除/清空；永久删除/清空后确认图片不在回收站与搜索中、原图仍在；清空操作记录后确认文件释放、`thumb://` 返回 404。
- **查看器与编辑**：打开/上下张/翻页、方向键、Esc、F/0/1/+/-、全屏、侧栏开关；旋转/翻转/裁剪（含 90° 方向的裁剪映射）/调尺寸/输出格式与质量；保存后原图替换、`img://` 立即刷新、标签/集合/描述保留；大图预览（>24MP、TIFF/TGA、EXIF 方向）首次生成延迟与 `temp/viewer/` 体积；4K 保存耗时与峰值内存；超过 50MP 的拒绝提示。
- **剪贴板**：复制图片/路径，含透明图与 EXIF 方向。
- **AI**：真实服务 health/ensure/embed/pose/describe 全链路、模型维度校验；窗口内任务进度、取消、失败清单、手写描述保护；语义搜索分页与 Ctrl+A；找相似五种 kind 与相似分组存为集合。
- **相似度阈值标定**：真实美术图上的 pHash 阈值（默认 6/256）、embedding 余弦阈值（默认 0.97）、主色距离阈值；20 万/50 万行下常驻视觉索引的内存占用。
- **剩余性能**：文件夹单级排序在 39 万行真实库上的首页耗时（20 万行为 42 ms，按 id 列表通道线性增长）；真实库 schema v3→v4 首次升级耗时；冷磁盘缓存下的缩略图加载。
- **失败内容**：`G:\Lilith Image Library Failed` 的 247 条超大图尚未缩图/重导入。
- **release 构建**：exe 与 DLL 清单（S9）。

## 下一步

1. 按上面的清单在 `npm run tauri dev` 下手工验收；真实库只读用于验收，测试、基准和演示仍用临时目录或复制样本。
2. 在便携目录中实测 AI 服务：设置 Python/脚本/模型路径，分别加载图片塔与文本塔，跑批量 embedding、pose、语义搜索，并观察空闲卸载。
3. 待决策项见 [待办与待决策](todo.md)；新阶段按 [路线图](roadmap.md) 继续。
4. 需要标定相似度阈值时，用复制出来的小样本跑基准，不要对 `G:\Lilith Image Library` 直接调参或写入测试数据。

## 约束

- `G:\Lilith Image Library` 已由迁移器改造为 Artbank 库；旧 `.data` 已删除，不要再让旧程序写入该目录。
- 应用不读旧 `.data`；迁移器在仓库外 `research/library-migration`，不进入 `src-tauri` 与产品提交历史。
- 真实库只用于验收；测试、基准和演示仍使用临时目录或复制样本，避免写入真实库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏。

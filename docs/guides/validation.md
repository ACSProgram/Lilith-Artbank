# 验证策略

## 允许与禁止

允许：

- `npm run typecheck`、`npm run build`、`npm run test:web`；
- `cargo fmt --check`、`cargo check`、`cargo test`、`cargo clippy --all-targets`；
- `git diff --check`；
- 对真实图库目录做**只读**清点（`ls`、文件数、尺寸），不写入。

禁止：

- 输入注入：`SendInput`、鼠标/键盘模拟、`pyautogui`、注入式消息，或任何驱动真实应用窗口的自动化；
- **UI 调试**：启动 `npm run dev` / `npm run tauri dev`、浏览器预览或无头 Chromium，截图、抓 DOM、量布局、看渲染性能都算；
- **编译 release**：`cargo build --release` / `npm run tauri build`，也不为了“确认能打包”而全量编译；需要时由用户自己执行；
- 修改 `G:\Lilith Image Library` 下的任何文件（包括“测试性改名/删除/写入”）；
- 把真实图库路径写进测试或基准；测试一律用 `tempfile` 临时目录。

界面视觉、手感与真实窗口行为全部由用户手工检查。代理只报告构建、类型、单测与基准的结论，并把验收步骤写清楚交给用户；未完成与待验收清单在[待办](../planning/todo.md)。

## 日常检查

```bash
npm run typecheck        # tsc --noEmit
npm run build            # tsc + vite build
npm run test:web         # node --test：几何换算、标签筛选、文件夹树、失效计划、范围、标签名
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
git diff --check
```

按改动选检查：

- 前端改动至少跑 typecheck；涉及构建配置时跑 build。前端纯函数改动（几何换算、标签筛选范围、排序换算、文件夹树、失效计划）跑 `test:web`；它用 Node 的类型擦除直接跑 `src/**/*.test.ts`，不引入测试框架依赖。
- Rust 改动至少跑 `cargo check` 与 `cargo test`；提交前跑一次 `cargo clippy --all-targets`。
- **导入**：`cargo test` 覆盖混合格式入库、精确/批内重复、合并策略、取消一致性、pack 缺失；吞吐跑 `perf_import_1000`。
- **浏览/查询**：`cargo test` 覆盖键集分页、多级排序、筛选、搜索、计数、全选 id、集合、子树筛选与直属筛选；性能跑 `perf_metadata_200k`。另有三类结构断言：`EXPLAIN QUERY PLAN` 不得再用 `idx_images_active_*` 回表（`active_count_plan_*`、`folder_counts_plan_*`）、`list_tags_page` 新形状与旧 SQL 逐行一致（回收站/临时区/阈值回退三种边界）、`folder_infos` 只回传请求的 id 且计数与整树一致。
- **元数据/回收站**：详情、评分/描述、标签增删改合并、文件夹移动回滚、软删除/恢复/永久删除/清空、延迟释放；文件操作测试只用临时图库。
- **查看器/编辑**：几何顺序（翻转→旋转→裁剪→缩放）、裁剪坐标、输出格式与 alpha、编辑上限；保存事务（新哈希发布、旧文件清理、元数据保留、`feature_blobs` 清空、冲突/无修改/回收站拒绝、只改质量也保存、体积预估与真实保存一致）；查看器源（直接服务、预览生成/复用/4096 上限、EXIF 方向、缺失原图）。前端同跑 `test:web` 与 typecheck/build。
- **AI**：`cargo test --lib ai::` 覆盖客户端 mock（health/embedding/describe/pose 解析、5xx 重试、4xx 不重试）、feature pack 追加与包头校验、均值中心化与 top-K、批量任务与手写描述保护；`rows_skip_trashed_and_temp_images` 固定“回收站与临时区被排除”的口径，`mean_of_chunk_skips_corrupt_records` 覆盖并行累加跳过损坏记录。
- **MCP**：`cargo test --lib mcp`，覆盖设置迁移与端口校验、Bearer 校验、selector 解析、三档阈值与确认过期、`library_info` 的 `writeAllowed` / `approval`、列表游标、标签缺失/排除筛选、`tags_modify` 与标签合并的撤销重做、`dryRun` 的 `willChange`、撤销/重做预览、导入输入预检与回执、临时区口径、`folders_modify` 四种动作、审计单行截断。
- **空间优化**：判据与阶梯编码、PSNR/坏点、EXIF/ICC 搬运、分层估算与只读、凭证指纹与候选集合核对、执行后只改内容列、旧图删除、校准记录。
- `server/` 只做一次性检查，不加入常驻测试：它依赖用户提供的 `python_embeded`，不能让日常测试在没有该环境的机器上失败。
- 提交前只核对一次 `git status` 与 diff，不反复刷状态。

## 数据库与文件

- schema/查询测试使用临时 SQLite 文件或内存库。
- 导入/编辑/清理测试使用小型确定性样本，重点验证：哈希、原子发布、不覆盖、取消边界、指针一致性。
- 不读取旧 `.data`；迁移脚本独立于应用。
- 200k 行基准会写入临时目录并在结束时删除；不要指向真实图库。

## 手工验收（真实窗口）

以下都由用户执行，代理不启动应用。逐条的验收清单在[待办](../planning/todo.md)对应条目里。

缩略图、浏览与导入：

```bash
npm run tauri dev
# 打开图库 → 导入图片/拖入图片 → 网格/列表滚动 → 筛选 → 选择 → 复制路径
# 观察 150 张缩略图从请求到可见的耗时、缓存命中与列布局
```

查看器与编辑：

```bash
npm run tauri dev
# 双击图片打开查看器 → 滚轮缩放/拖拽/双击/全屏（F11、Esc 先退全屏）→ ←/→ 或翻页 →
# 旋转/翻转/裁剪（先裁剪再旋转，确认选区跟着转）→ 调尺寸 → 撤销/重做 → 选择输出格式与质量
# 确认：输出区显示“保存后约 xx（原 xx，-xx%）”；只改 JPEG 质量也能保存
# 保存后：原图文件替换、缩略图与查看器立即刷新、网格不回到第一页、标签/评分/描述/集合保留
# 右键“复制图片” → 粘贴到画图/聊天工具；大图与 EXIF 方向图确认显示与裁剪一致
```

AI 服务与语义检索：

```bash
npm run tauri dev
# 设置 → AI 服务 → 检测/启动 → 确认状态行显示图像塔/文本塔/姿势的“已加载 / 未加载”
# 确认设置页没有手动加载模型的按钮；模型由首次使用自动加载
# 选中图片或当前视图 → AI 任务 → embedding/pose 批量任务（确认“开始任务时自动加载”）
# 确认进度、取消、失败清单；搜索框切到“语义”输入英文短语并 Enter，首次搜索应提示正在加载文本模型
# 查看器无 pose 时点击“生成姿势骨架”
# 右键图片“设为相似参考” → 工具栏“相似”打开面板 → 查找相似 → 点击结果条里的比较图文件名打开查看器
# 描述未实现：批量任务里的描述项应置灰
```

MCP 真实客户端：

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
13. `find_similar` 分别传 `scope: {"type":"folder","folderId":…,"includeSubfolders":false}` 与 `targetIds`（两张图，`kind: color8`），确认结果只来自范围、参考图全部被排除。
14. `import_start` 传 `target: "temp"` 导入几张图：询问档应直接开始、不出现确认卡；`tasks_get` 回执 `target=temp`、`operationId=null`；`search_images` 用 `view: "temp"` 能看到图片，普通 `view: "all"` 看不到。
15. 对 `view: "temp"` 的图片调用 `images_update` 改评分，`history_list` 不应新增记录；调用 `images_trash` 应得到明确拒绝。
16. 把 3 张已在库内的图（其中 1 张在回收站）重新导入一次，`tasks_get(includePairs=true)` 返回 3 条 `duplicatePairs`，`imageId` 指向首次入库的图，回收站那张 `trashed=true`。
17. 对同样 3 张改用 `duplicatePolicy=merge` 重跑，`mergedImageIds` 与上一步的 3 个 id 一致，`images_get` 能看到标签并集与较高评分。
18. 同一目录放 2 份完全相同的图导入，`batchDuplicatePairs` 给出一对 `keptPath` / `sourcePath`，`duplicates=1`。
19. 临时区导入 5 张图后调用 `temp_features_generate(kind=embedding)`：询问档出现确认卡，批准后任务占 `ai.service` 运行；完成后 `library_info.features.embeddings` 不变、`tempEmbeddings=5`。
20. 以临时图为参考、`scope=temp` 搜语义能命中临时区内的图；改成 `scope=library` 后结果里不出现任何临时图。清空临时区后再搜语义不报错。
21. `ids` 里混入一张普通库图片，`temp_features_generate` 整批拒绝并列出 `invalidIds`；生成中途取消后已完成张数保留、任务为 `cancelled`。
22. `tags_list` 不带 `limit`：返回 100 条 + `total` + `nextCursor`；把 `nextCursor` 传给 `cursor` 再调一次，拿到剩余标签且没有重复 id。
23. `search_images` 传 `filters.tagPresence: "missing"` 只能拿到没有标签的图片；再加 `excludeTagIds`，结果里不再出现带该标签的图片。
24. 切到完全档调 `library_info`：`approval.threshold` 为 `null`、`appliesToCurrentMode` 为 false；问 AI「完全档批量写入需不需要确认」，应答不需要。
25. `tags_modify` 合并两个近义标签：先 `dryRun` 看 `moved` / `duplicate`，正式执行后源标签消失、目标标签图片数为两者之和；`history_list(kind: "tagMerge")` 有 `AI ·` 记录，撤销后两个标签与关系都回来。

便携包：

```bash
npm run pack:portable
# 把 python_embeded\ 与 resources\models\ 放进 release\Lilith Artbank\ 后直接运行 lilith-artbank.exe
# 确认 settings.json 与 logs\ 生成在 exe 旁，不出现 NSIS 安装器
```

## 性能基准

默认被 `#[ignore]` 标记，按需运行：

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_metadata_200k
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_thumb_pack_read
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_import_1000
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_history_200k
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_encode_threads
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_optimize_batch_size
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_estimate_vs_actual
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_estimate_sampling_spread
cargo test --manifest-path src-tauri/Cargo.toml --lib ai::embedding::perf_tests -- --ignored --nocapture
```

最后一项会生成约 614 MB 的临时 fp16 向量包，结束后删除；只用于 20 万条 × 1536 维扫描基准。

浏览器缩略图基准：

```bash
python tools/perf/thumb_bench/generate.py
node tools/perf/thumb_bench/server.mjs
# 打开 http://127.0.0.1:18767/?format=jpg 与 ?format=webp，看页面右下角结果
```

规则：

- 同一台机器、同一构建档、同一浏览器才有可比性；不要用跨机器墙钟下结论。
- 结果写入[性能基准](performance.md)的实测记录，注明日期、机器、构建档与浏览器版本。
- 新增热点路径必须补对应基准或说明为什么不影响门槛。
- 代理不跑 `npm run dev` 做浏览器预览检查；布局与观感由用户验收。

## AI 服务端一次性检查

不进入常驻测试：依赖用户提供的 `python_embeded`，不能让日常测试在没有该环境的机器上失败。

```bash
python_embeded\python.exe -m py_compile server\main.py server\clip_cache.py server\pose_engine.py server\image_io.py
python_embeded\python.exe server\main.py --port 18766 --model-dir resources\models\siglip2-giant-opt-patch16-384
curl.exe http://127.0.0.1:18766/v1/health
# 另开一个终端：
curl.exe -X POST -H "Content-Type: application/json" -d "{\"capabilities\":[\"imageEmbedding\"]}" http://127.0.0.1:18766/v1/models/ensure
curl.exe -X POST http://127.0.0.1:18766/v1/shutdown
```

- 图片塔与文本塔应分别加载；加载另一塔时旧塔让位。
- 空闲 300 秒后 health 的两个塔都变回 `ready=false`；可用 `--idle-unload-seconds` 调短验证。
- Python 服务与模型路径必须由用户环境提供，不要写成 CI 或常驻测试。

## 发布产物依赖检查

```bash
cargo build --manifest-path src-tauri/Cargo.toml
python tools/perf/pe_imports.py src-tauri/target/debug/lilith-artbank.exe
```

脚本列出 PE 的 import 与 delay-import DLL。当前只包含系统 DLL，没有 `WebView2Loader.dll`、`vcruntime140.dll` 或 `webp.dll`；release 产物的最终核对还没做。

## 收尾

- 未完成与待验收事项写 `docs/planning/todo.md`（唯一未完成清单）；当前能力与基线写 `docs/planning/current-handoff.md`。
- 批次记录与结束的计划移入 `docs/planning/archive/`，并更新其 README 索引；不新建阶段计划文件。不要把“计划实现”写成“已验收”。
- 性能门槛以[性能基准](performance.md)为准；未达门槛先解决，再继续堆功能。

# 当前阶段状态

日期：2026-10-24
阶段：**单库与多级文件夹 A–C 已实现并通过自动验收；D 旧库全量迁移已完成（主库 390 542 张、独立测试库 9 455 张、247 条失败内容移出库外），两库自动验证通过；E 自动检查已运行，真实窗口待用户验收；S7 相似度检索与相似分组已实现并通过自动验收，真实窗口与 20 万级基准待验收；体验整改 P0 基础层已完成并通过用户手工验收，P1–P4 待执行**

历史批次（S0–S3）的实现与验证记录已移入 [归档](archive/2026-10-23-S0-S3-实现记录.md)；查询引擎与缩略图基线仍以 [性能基准](../guides/performance.md) 为准。

## 当前批次：体验整改 P0 基础层（2026-10-24）

来源与后续批次见 [迁移后体验整改计划](2026-10-24-体验整改计划.md)。本批只铺地基，不改用户可见流程（唯一可见变化是主题真正生效、移入回收站开始确认）。

Rust：

- `app/settings.rs`：设置版本升到 3。`UiSettings` 新增 `thumbFit`、`thumbCacheCount`、`sort`、`sortProfiles`、`viewerFilmstrip`、`detailPanelMode`；删除 `PerformanceSettings.thumbnail_cache_mb`；`theme` 增加 `system` 并作为默认值。新增 `SortSpec` / `SortProfile` DTO 与校验：主题三选一、填充两选一、缓存张数 256-8192、详情面板两选一、排序 1-7 级且指标不重复、方案 ≤50 个且名称不区分大小写唯一。
- `app/settings.rs` 迁移：`parse_settings` 改成 v1→v2→v3 链式，`migrate_v2` 删掉 `performance.thumbnailCacheMb`，其余字段由 serde 默认值补齐，旧值保留。
- `library/schema.rs`：schema 版本升到 3。`folders` 加 `sort_index INTEGER NOT NULL DEFAULT 0`；`migrate_v2_to_v3` 用 `ROW_NUMBER() OVER (ORDER BY path COLLATE NOCASE, id)` 按名称编号，升级后顺序不变；`migrate_v1_to_v2` 只升到 2，由 `ensure` 继续链到 3。

前端：

- `src/styles/tokens.css`（新）：浅色 `:root` 与 `html[data-theme="dark"]` 两套语义变量，覆盖表面/文字/强调/状态/遮罩/画布/缩略图占位/阴影；填充色（`--accent-fill` / `--danger-fill`）与文字色分开，避免深色下白字压浅色按钮。
- `src/styles/index.css`：`@import "./tokens.css"`，80 余处字面色值全部换成变量（含查看器/编辑器原来的整套深色硬编码、三处 `rgba()` 阴影），`grep -rn "#[0-9a-f]\{3,8\}\|rgba(" src/styles` 现在只命中 tokens.css。设置页的静态主题色块换成三个主题选项按钮（`.theme-options` / `.theme-option`），另加 `.confirm-detail`、`.confirm-checkbox`、`.popover`、`.menu-section*`、`body[data-scroll-lock]`。
- `src/app/theme.ts`（新）：解析 `light` / `dark` / `system`（`system` 监听 `prefers-color-scheme`），写 `html[data-theme]`；`main.tsx` 在首次渲染前调用 `initTheme()`，`AppRuntimeProvider` 在设置变化后重新应用，`SettingsDialog` 打开时按草稿即时预览、取消则退回已保存值。
- `src/app/types.ts` / `api.ts`：`Settings` 对齐 v3，默认 `theme=system`、`thumbFit=cover`、`thumbCacheCount=1024`、`sort=[{importedAt,desc}]`、`detailPanelMode=overlay`；`SettingsDialog` 删掉“缩略图缓存 (MB)”输入与校验。
- `src/shared/components/Modal.tsx`：补焦点陷阱（Tab/Shift+Tab 只在面板内循环）、背景滚动锁、初始焦点选项；开着的弹窗按栈记录，只有最上层响应按键。`onClose` 放进 ref，避免重渲染把焦点抢回面板。
- `src/shared/components/ConfirmDialog.tsx` + `src/app/noticeContext.ts`：`ConfirmOptions` 增加 `detail` 与 `checkbox`（复选框状态由调用方持有，返回值仍是 boolean）。
- `src/shared/confirm.ts`（新）：`confirmTrash` / `confirmDelete` / `confirmEmptyTrash` 语义化文案。
- `src/shared/components/Popover.tsx`、`MenuSection.tsx`（新）：排序/筛选/菜单共用弹层与菜单分组标题；本批暂无使用点。
- `src/modules/library/LibraryView.tsx`：`trashSelected` 统一加 `confirmTrash`，覆盖右键菜单、Delete 键、详情侧栏、查看器四个入口；`deleteSelected` / `emptyTrash` 改用语义化封装。

文档：

- `library.md`：设置 v3 字段与校验、v2→v3 迁移、schema v3 与 `folders.sort_index` 的现状（手动排序命令尚未实现）。
- `browse-and-query.md`：回收站确认的收口点与三种确认语义。
- `overview.md`：分层补 `src/styles/`、主题与确认的前端约定、两条取舍记录、P0 实现状态；`ai-reading-guide.md` 补颜色与确认文案的边界。

### 提交前复核（2026-10-24）

- `Modal` 的按键处理改成叠层栈：只有最上层响应 `Esc` / `Tab`，滚动锁跟随栈空/非空。原先只用计数锁滚动，在设置对话框上再开确认框时，两个弹窗会抢同一套按键（`Esc` 会关掉被遮住的那一层，`Tab` 会被抢回底层）。
- `initialFocus` 由 `first` 改为 `panel` / `footer`：确认框原来聚焦的是标题栏的关闭按钮，现在停在“取消”上。
- `theme.ts` 收掉外部没有使用的 `resolveTheme` / `currentThemeMode` / `ResolvedTheme` 导出；重复设置同一个值不再写 `data-theme`。
- `Popover` 修正 `closeOnScroll` 的注释（默认值是 `true`）；上下翻转后统一夹进视口，弹层比视口高时不会再跑到负坐标。
- Rust：`MAX_SORT_LEVELS` 改为由 `SORT_KEYS.len()` 推导，指标增删时不会和级数上限脱节；排序方案内的排序校验失败时，报错带上方案名；schema 迁移用 `mark_version` 统一写 `user_version` 与 `meta.schemaVersion`。

### 验证结果（P0）

```text
npm run typecheck
npm run build
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets   # 无警告
cargo test --lib             # 131 passed, 0 failed, 5 ignored
git diff --check
grep -rn "#[0-9a-f]\{3,8\}\|rgba(" src/styles   # 只命中 tokens.css
```

新增测试：设置 v2→v3 迁移（`thumbnailCacheMb` 被删除、旧值保留、新字段默认值）、非法主题拒绝、重复排序指标拒绝、schema v2→v3 迁移按名称编号、新库 `folders.sort_index` 默认可写。

用户手工验收（2026-10-24）：`npm run tauri dev` 下浅色/深色/跟随系统切换、查看器与主界面同色系、四个回收站入口确认、弹窗 Tab 焦点循环均通过。

遗留：

- 逐页截图检查未做（本机没有浏览器自动化依赖），两套主题的观感由用户手工确认。
- 主题之外的六个新设置字段、`Popover`、`MenuSection` 尚无消费者，随 P1/P3/P4 接入。

本批已作为一个提交落地（见 `git log`），未推送。

## 此前批次：相似度检索与相似分组（S7）

2026-10-24 完成，与 D 迁移并行；只改应用代码，未触碰 `G:\Lilith Image Library`。

Rust：

- `library/similarity.rs`：`VisualIndex` 常驻 pHash（4×u64）与 4/8 色 OKLab（20 万行约 46 MB），用 `count(*) + max(id)` 判断是否过期；`similar_phash`（Hamming 升序）、`similar_color`（对称加权 OKLab 距离）、`phash_groups`（8×32 位分段索引 + 并查集；阈值 ≤7 不漏候选，组过大/检查超限/组数超限只标记 `truncated`）；`target_visuals` 只接受未删除且哈希、主色完整的图片。
- `ai/pose_similarity.rs`：原程序 `MediaPipePoseData::similarity` 1:1 移植（躯干、四肢 2D/3D、PCA、径向签名、凸包面积、镜像取较大值），9 组 golden 值与 MSVC 编译的原实现相差 <1e-12。
- `ai/embedding.rs`：`target` 读目标向量、`similar_to` 减均值扫描 top-K、`refine_groups` 用中心化余弦把 pHash 候选拆成语义一致的小组。
- `ai/pose.rs`：`similar` 每个包 mmap 一次后顺序切片打分。
- `ai/pack.rs`：`record_slice` 上移，embedding 与 pose 共用。
- `app/runtime.rs`：`find_similar_images`（5 种 kind）与 `find_similar_groups`（pHash / embedding 复核）；`SemanticCache` 改为 `ResultCache`，语义搜索与找相似共用同一个 token 槽；`invalidate_semantic` 改名 `invalidate_similarity` 并同时清掉视觉索引。
- 新命令：`find_similar_images`、`find_similar_groups`。

前端：

- `src/app/useSimilarityBrowse.ts`（原 `useSemanticSearch.ts`）：`mode: semantic | similar`，共用分页、全选与重试。
- `src/app/SimilarityGroupsDialog.tsx`：pHash/embedding 扫描、阈值、截断与缺失提示、每组存集合与合并保存（≤100 组）。
- 右键菜单新增“找相似”五项（回收站图片不显示）；工具栏新增“相似分组”；结果条按模式切换文案与图标。
- `CollectionManagerDialog` 新增“从当前结果创建集合”；`src/shared/similarity.ts` 收敛 kind 与中文标签。

### 验证结果（S7）

```text
npm run typecheck
npm run build
cargo fmt --check
cargo check
cargo clippy --all-targets   # 无警告
cargo test --lib             # 126 passed, 0 failed, 5 ignored
```

本轮收尾整理（不改行为契约）：

- `ai/pose_similarity.rs`：删掉遗留的 `POSE_DEBUG` 调试输出；`denom23` 与 `limb_total` 的重复定义收敛为一个；golden 值测试的说明移到属性之前。
- `imageops/colors.rs`：删掉测试专用的 `palette_distance` / `palette_lab` / `nearest_weighted` 第二套距离实现，测试直接走 `prepare_palette` + `prepared_distance`。
- `library/similarity.rs`：删除 `palette_bytes` 别名；`MAX_GROUPS` / `MAX_GROUP_SIZE` 提为模块常量供 embedding 复用；`UnionFind` 提为 `pub(crate)`；新增 `ensure_visible`，pose 找相似不再要求目标图有完整主色。
- `ai/embedding.rs`：`vectors_for` 的分段查询改成 `types::Value` 绑定（原先 dim/id 以文本绑定，靠 SQLite 亲和性隐式转换）；新增 `read_vector` 供 `target` 与 `vectors_for` 共用。
- `ai/pose.rs`：pose 相似度从“每条记录打开一次文件”改成每个包 mmap 一次再切片；上限常量复用 `library::similarity::MAX_LIMIT`。
- `ai/runtime.rs`：`color4` / `color8` 两个分支合并。
- 前端：`LibraryView` 去掉 `const semantic = similarity` 别名，`semanticActive` / `semanticKey` / `semanticClear` 统一为 `similarity*`，加载与空态文案按 `similarMode` 分支；`useSimilarityBrowse` 复用图库 `PAGE_SIZE`；`ImageSimilarityResult.kind` / `SimilarityGroupsResult.kind` 改为联合类型，结果标签改用 `result.kind`（切换下拉不再沿用上一次的标签）。
- `src/styles/index.css`：补 `.dialog-field` 与 `.similarity-group-*` 样式（原先这些类没有任何规则，分组列表会挤成一列）。
- 文档：`ai-service.md` 补本地检索与命令表、删除“没有 embedding 时先调用服务生成”的错误描述；`overview.md` 删掉不存在的 `src-tauri/src/features/`；`browse-and-query.md` 补找相似入口与限制；`roadmap.md` 标记 S7 已实现；`todo.md` 换成收尾项。

## 此前批次：旧库全量迁移（D，2026-10-24）

迁移器不属于产品代码，位于仓库外 `research/library-migration/tools/migrator`（主仓库 `.gitignore` 排除 `/research/`）。真实库已完成改造，用户手工验收待执行。

- 旧格式：读取 zstd 压缩的 `image_data.bin` / `saved_lists.dat`，解析旧 pHash、扩展名、一级 folder、秒级导入时间、评分、BGR 主色、标签、描述、可选 33 点 f64 pose；颜色与 pHash 不迁移，按新算法重算。
- 迁移：BLAKE3 内容寻址；确定性选主；标签并集、评分取高、导入时间取早、描述保留首个非空并报告冲突；pose 转 f32；缩略图重建为 512px WebP。
- 可恢复：rename journal、ledger、`--resume`、`verify --hash`、`rollback`、`cleanup`；失败项保留原文件。
- 新增 `--single-library`（根目录即图库）与 `quarantine-failures`（失败图片连同文件夹/元数据移到库外）。
- 主库 `G:\Lilith Image Library`（`Lilith Artbank`）：393 151 条旧记录、390 788 个唯一内容、2 363 条重复、390 542 张入库、246 条失败、81 649 条 pose、556 个标签、38 个文件夹、9 个缩略图段；耗时 17 230.3 秒（约 4 小时 47 分）。
- 测试库 `G:\Lilith Test Library`（`Lilith Artbank Test`）：旧 `测试` 整体移出主库后以单库模式迁移，9 455 张、0 失败/重复、87 条 pose、30 个孤儿；耗时 147.6 秒。
- 失败内容：246 条超过 1 亿像素的记录 + 1 条失败计划遗留的未引用副本，共 247 条、7.3 GiB，已移到 `G:\Lilith Image Library Failed`；`manifest.jsonl` 保留旧 hash、BLAKE3、文件夹、标签/评分/描述/pose、尺寸与失败原因。
- 清理：两库旧 `.data`、`recycle`、`output`、`temp`、孤儿/重复隔离区已删除（主库约 41.7 GiB）；库内 `.artbank/migration` 已删除，审计副本在 `research/library-migration/data/full-run/`。
- 应用设置 `libraryPath` 已指向主库；试点副本 `data/pilot` 已删除。
- 详细结果：`research/library-migration/docs/05-full-run-results.md`；迁移计划已归档。

## 已完成批次：单库与多级文件夹（A–C）

Rust：

- 设置 v2：只保存 `libraryPath`；v1 的 `libraries[]` / `lastLibraryId` 读取时迁移；设置页提交不回写图库路径。
- schema v2：`folders(path COLLATE NOCASE)` 唯一索引；v1→v2 迁移检测大小写冲突并拒绝打开冲突库。
- 单库生命周期：`create_library` / `open_library_path` / `close_library` / `rename_library`；启动自动打开上次图库；删除多库列表与移除命令。
- 文件夹服务：路径规范化（Windows 非法字符、保留名、保留目录）、祖先补齐、创建/重命名/移动/删除空文件夹；物理目录与数据库事务同步、失败回滚、仅大小写重命名。
- 查询：`folderIds` 改为子树语义（路径边界）；`get_folders` 返回直接/子树计数；文件夹分面按子树展开。
- 导入：`folderMode=flat|preserve`；扫描记录相对目录；写线程缓存 `folder_id`。
- 性能基准：200k 图片 + 10,221 个文件夹；子树页（顶层文件夹、约 1 万张）4 ms、`get_folders` 25-29 ms、子树条件下文件夹分面 62-66 ms（p95 峰值 119 ms，在 100 ms 预算边缘，面板保留加载态）。

前端：

- 单库外壳：欢迎页创建/打开、启动自动打开、设置页更换图库、关闭图库；移除多库列表。
- 文件夹树（多选、展开、右键菜单、拖放）、面包屑、通用文件夹选择器、内部拖放 MIME。
- `BrowseState` 提升到 `LibraryWorkspace`；树、网格、面包屑、选择器共享 `filters.folderIds`；筛选面板移除文件夹分面。
- 导入与详情批量移动复用树选择器；浏览器 mock 支持文件夹 CRUD、空文件夹、子树筛选与拖放。

## 验证结果（A–C）

```text
npm run typecheck
npm run build
cargo fmt --check
cargo check
cargo clippy --all-targets   # 无警告
cargo test --lib             # 112 passed, 0 failed, 5 ignored
cargo test --lib -- --ignored --nocapture perf_metadata_200k
git diff --check
```

新增测试覆盖：设置 v1 迁移与后端路径保护；schema v1→v2 与大小写冲突；文件夹路径校验、多级创建、重命名/移动子树、仅大小写重命名、空文件夹删除；子树查询与相似前缀不误匹配；直接/子树计数；导入 `preserve` 同名子目录合并。

## 验证结果（D 全量）

```text
cargo fmt --check / clippy --all-targets / test   # research 迁移器：14 passed，无警告
migrate 主库                                      # 390542 张、2363 重复、246 失败、81649 pose、17230.3s
verify --hash 主库                                # integrity=ok；缺失/大小/元数据/缩略图/pose/哈希=0
quarantine-failures 主库                          # 246 条 -> G:\Lilith Image Library Failed
verify 主库（清理后最终）                          # 全部 0；未引用文件 0
migrate --single-library 测试库                    # 9455 张、0 失败/重复、87 pose、147.6s
verify --hash 测试库                              # integrity=ok；全部 0；未引用 0
cleanup 两库                                      # 旧 .data/recycle/output/temp/隔离区已删除
```

- 全量 `verify --hash` 之后只移出了 1 个未被数据库引用的失败记录副本（设计库，38 MB），
  已引用原图未再改动；最终复验未重算 690 GiB 原图哈希。
- 审计副本：`research/library-migration/data/full-run/main/` 与 `test/`；
  详细结果：`research/library-migration/docs/05-full-run-results.md`。

浏览器预览已完成静态/交互检查（mock 数据）：单库创建、树计数、子树筛选、多选、文件夹 CRUD、图片/文件夹拖放、面包屑、Inspector 选择器。未取得截图，真实 WebView2 布局与交互待用户验收。

本轮收尾整理（不改行为契约）：

- `paths.rs`：删除过期的“Stage 1/3 才接入”说明与整模块 `allow(dead_code)`；移除未被引用的 `thumb_segment`/`feature_file`、以及祖先补齐已由 `ensure_folder` 负责的 `folder_ancestors`；`escape_like` 与新增的 `subtree_like_pattern` 收敛到一处，供查询与文件夹服务共用，并补单测。
- `query.rs`：子树条件提为常量 `FOLDER_SUBTREE_SQL`；`list_folders` 复用 `expand_folder_counts`，不再重复“沿路径累加到祖先”的实现；`all_folders` 按 `path` 排序，保证返回值稳定。
- `folders.rs`：新建只调一次 `ensure_folder` 补齐祖先（原先先遍历祖先再调一次）；删除子文件夹的计数改用共享子树模式。
- `import/pipeline.rs`：`prepared`/`duplicates`/`folder_cache` 收敛为 `WriteBuffer`，`flush` 参数从 8 个降到 6 个并去掉 `too_many_arguments`；`folderMode` 分支合并为两条。
- `import/scan.rs`：跳过目录改用 `paths` 常量，并补上计划 3.2 要求的旧程序 `.data`；测试覆盖。
- `library/perf.rs`：子树页基准原先取的是叶子文件夹 id（`folder_ids: vec![7]`，实际约 20 张），与文档“选中一个顶层文件夹”不符；改为按路径取 `folder0` 的 id 并断言填满一页。复测后子树页 4 ms、`get_folders` 25-29 ms、子树分面 62-66 ms，原 0.37 ms/33 ms 是叶子口径的失真数据。
- 前端：`ancestorPaths` 复用到树展开与面包屑（原先三处各自切分路径），删除重复的 `inFolderSubtree`；批量移动在文件夹选择器里选根目录时改走“移动到根目录”，不再误报“请输入目标文件夹”；面包屑只有最深一级标为当前；合并 `LibraryWorkspace` 重复的类型导入，`FolderTree` 去掉未用的 `FolderTreeModel`。
- 文档：`overview.md` 删除不存在的 `src/modules/editor` 与 AI 模块的“（后续）”，标注图像处理已取消；`import.md` 补 `.data`；`performance.md` 与 `browse-and-query.md` 更新子树数字并把基准数字收敛到一处；本文件同步。

## 此前批次：S6 AI 客户端与语义搜索

### 前提

- 跳过 S5（色彩/LUT/DFT/卷积），原因与作废计划见 [S5 取消与 AI 模型调整](archive/2026-10-23-S5取消与AI模型调整.md)。
- image/text embedding 改为 `siglip2-giant-opt-patch16-384`，1536 维，落盘 fp16；旧 CLIP 向量不兼容、不参与检索。
- 语义检索在打开图库时计算全库 embedding 均值，查询使用 `cosine(v - mean, q - mean)`；会话内不实时更新。
- 向量包 mmap 分块扫描，不整体载入内存；结果只回 id + 分数。
- pose 继续用 MediaPipe Pose，S7 相似度按原程序 `ZyImageAlgorithm.cpp` 1:1 移植，不改权重与公式。
- description 换模型但 `/v1/describe` 契约不变；本批只做客户端，不实现服务端。

### 完成内容

Rust：

- `ai/client.rs`：`/v1/health`、`/v1/models/ensure`、`/v1/embed/image`、`/v1/embed/text`、`/v1/pose`、`/v1/describe`、可选 `/v1/shutdown`；连接超时 5s，embedding/pose 60s、description 300s；网络错误与 5xx 重试 2 次，4xx 不重试。
- `ai/process.rs` + `ai/runtime.rs`：`executablePath` 启动、`--port`、健康检查、最多 3 次退避重启、`autoStart`、手动启动/停止、退出时优雅关闭后终止；外部服务只连接不接管。
- `ai/pack.rs`：`features/<kind>-<model>-<dim>-<dtype>-<hash>.pack` 追加式写入，32 字节包头（magic/version/kind/dtype/dim/record_len/created_at）。
- `ai/embedding.rs`：服务返回向量再归一化后存 fp16；打开图库后台计算全库 fp32 均值；检索使用 `cosine(v - mean, q - mean)`，mmap + rayon 分块扫描 + top-K；`feature_blobs` 只存指针。
- `ai/pose.rs`：33 点 × (x,y,z,visibility) 存 f32；空结果按 33 个零置信点保存。
- `ai/job.rs`：embedding/pose/description 批量任务；按选中/当前视图/全库解析 id；默认只补缺失，`overwrite` 只重算非手写描述；任务可取消，进度 150ms 节流，失败清单最多 200 条；连续 5 次失败停止。
- `ai/input.rs`：embedding/pose 使用 512px 缩略图包，缺失时回退解码；description 解码原图并生成最长边 1536 的 WebP。
- `metadata::set_ai_description`：写 `description_source='ai'`、`description_model`；空描述可填，非空手写描述永不覆盖。
- 新增命令：`get_ai_status`、`ai_ensure_models`、`ai_start_service`、`ai_stop_service`、`start_ai_job`、`cancel_ai_job`、`get_ai_job_status`、`ai_describe_image`、`ai_generate_pose`、`semantic_search`、`get_similarity_page`、`get_similarity_ids`；`get_image_pose` 改为读取 feature pack。
- `query::rows_by_ids`：语义结果按 rank 顺序回填完整行。
- 图库打开/关闭接入 AI 运行时的均值与语义缓存；编辑、移入/恢复/删除/清空回收站会使语义缓存失效；AI 任务运行期间禁止关库/导入/编辑。

前端：

- `src/modules/ai/`：types、api、AI 状态轮询、任务对话框；服务状态、模型加载、范围选择、覆盖选项、进度/取消/失败清单。
- 图库工具栏：AI 状态胶囊、AI 任务入口、语义搜索面板与结果模式；语义模式禁用普通筛选/排序，Ctrl+A 选择全部语义结果。
- 详情侧栏：单张“AI 生成描述”，成功后立即显示并刷新。
- 查看器：无 pose 时可“生成姿势骨架”，有 pose 时切换叠加；返回 33 点为零时提示未检测到人体。
- 设置页：AI 服务检测/启动/停止/加载模型与 health 能力展示。

### 验证结果（S6）

```text
npm run typecheck
npm run build
cargo fmt --check
cargo clippy --all-targets   # 无警告
cargo test --lib             # 99 passed, 5 ignored
```

新增测试：

- 客户端：health/embedding 解析与归一化、5xx 重试、4xx 不重试、describe 查询参数编码、pose 33 点。
- job：mock 服务下 embedding/pose/description 端到端；手写描述在 overwrite 下仍不被覆盖。
- pack：追加/读取/越界/包头一致性；pose 缺失点补零。
- embedding：均值中心化排序、回收站图片排除、top-K 只返回前 K 条且按分数排序。
- 基准：20 万条 × 1536 维 fp16 单 pack 扫描 449.26 ms（开发档），低于 500 ms 预算。

浏览器静态检查未能完成：内置浏览器导航 `http://127.0.0.1:5173` 连续超时、页面为空白；未使用真实窗口或输入自动化。语义/任务界面的实际布局仍需用户在 `npm run tauri dev` 中手工验收。

本轮收尾整理（不改行为契约）：

- 客户端：去掉恒为空的 `normalized`/`truncated` 占位逻辑（截断改为记日志）；`with_retry` 去掉不可达的兜底分支；`describe` 的查询参数构造简化；百分号编码不再逐字节 `format!`；L2 归一化收敛到 `embedding::normalize_f32` 一处实现。
- 检索：top-K 改为 `select_nth_unstable_by` 部分选择后再排序，与文档一致，不再为取 K 条对全量结果排序；新增对应测试。
- 运行时：`MeanSlot` 增加 `ready/empty/failed` 构造，均值状态的四处字段赋值收敛；`status()` 只取一次进程锁；监督线程改用 `SupervisorGuard` 按代次释放标记，避免旧线程清掉新线程的 `supervising`；启动次数上限提取为 `process::MAX_START_ATTEMPTS`。
- 任务：`existing_features` 的 kind 改为绑定参数、分支合并；描述状态改为 `DescriptionState`；描述长度默认值收敛为 `job::DEFAULT_DESCRIBE_TOKENS`；去掉重复的失败路径克隆。
- 命令：`ai_stop_service` 改为异步 + 阻塞线程池，停止服务不再卡住 IPC 线程。
- 前端：`aiApi` 用 `requireTauri`/`offlineStatus()` 去掉 13 处重复分支；语义结果与图库查询在 `LibraryView` 合并为单一 `listSource`；`useSemanticSearch.retry` 去掉走不到的 token 分支；任务对话框重新打开时接管 `status.job`，并补上阶段中文标签；设置页去掉“接入后……”等过期文案。
- 文档：修正 pack 命名（含 hash 后缀）、pose 的 dim 语义、进度节流 150ms、启动失败重试与“运行中崩溃不自动重启”的实际行为、`features/*.bin` 笔误；路线图标记 S6 已实现；清理交接文档中 S5/S6 的过期结论。

## 更早批次：S4 查看器与几何编辑

Rust：

- `imageops/edit.rs`：翻转 → 顺时针旋转 → 裁剪 → Lanczos3 缩放的几何算子；JPEG/PNG/WebP 编码，JPEG 透明合成白底，编辑上限 5000 万像素/单边 65535。
- `library/viewer.rs`：`get_viewer_source`。小图直接由浏览器解码；>2400 万像素、TIFF/TGA 或带 EXIF 方向的图片生成最长边 4096 的 WebP 预览，缓存于 `.artbank/temp/viewer/`。
- `library/edit.rs`：保存采用“新内容发布”事务——临时编码 + BLAKE3 + `sync_all`、冲突检查、原子发布、追加缩略图、更新 `images` 行并清空 `feature_blobs`，提交后删除旧原图与旧预览；标签/评分/描述/集合保留。
- `library/content.rs`：原图行加载与路径解析。
- `app/img_protocol.rs`：`img://`（Windows 为 `http://img.localhost`）提供原图与预览，带 immutable 缓存与内容哈希 ETag。
- `platform/clipboard.rs`：`copy_image` 解码、应用 EXIF 方向后写入系统剪贴板。
- 新增命令：`get_viewer_source`、`save_image_edit`、`get_image_pose`（S6 前返回 null）、`copy_image`、`window_toggle_fullscreen`。保存与导入/图库切换共用维护守卫，并在阻塞线程池执行。

前端：

- `src/modules/viewer/`：types、api、model、`ImageViewer`、`ViewerTools`、`PoseOverlay`。适应窗口/1:1/滚轮缩放、拖拽平移、双击切换、上下张与翻页加载、全屏、键盘快捷键、裁剪选区、输出格式与质量。
- `src/app/ViewerHost.tsx`：查看器与可编辑详情侧栏的编排；评分/标签/描述/文件夹直接复用 Inspector，回收站/恢复/永久删除成功后关闭查看器。
- `src/shared/viewerSession.ts`：图库向查看器提供已加载条目、翻页、选择同步与元数据操作的桥接契约。
- `src/shared/imgUrl.ts`：`img://` URL 生成。
- 入口：双击网格/列表、Enter、右键“打开查看器”；网格右键新增“复制图片”。
- 拖出到资源管理器（Windows OLE）按用户确认暂缓，保留“复制图片”与“复制文件路径”。

## 验证结果

已运行并通过：

```text
npm run typecheck
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets   # 无警告
cargo test --manifest-path src-tauri/Cargo.toml --lib   # 83 passed, 4 ignored
git diff --check                                        # 无空白问题
```

S4 新增测试覆盖：

- 几何算子：翻转先于旋转、旋转后裁剪坐标、裁剪后缩放、非法参数拒绝；三种输出格式可解码，PNG 保留 alpha，JPEG 透明合成白底。
- 保存事务：宽高交换、旧原图删除、新文件存在、缩略图指针可读、`thumb_version` 递增；标签/评分/描述/集合保留、`feature_blobs` 清空；哈希冲突、无修改与回收站图片被拒绝。
- 查看器源：小图直接服务、预览生成与复用、4096 长边上限、EXIF 方向 6 生成转正预览、原图缺失报错、回收站只读。

本轮收尾整理（不改行为契约）：

- 修复 `img://` 预览响应的 `Content-Type`（`webp` → `image/webp`）；图片行缺失改为 404，与前端失败态一致。
- 查看器打开时把焦点移进覆盖层，避免网格容器级键盘处理与查看器同时响应 ←/→、Esc；覆盖层补 `aria-modal`。
- 去掉重复的 URL 重试参数、未使用的 `zoomPercent`/`activeItem`、`has_changes` 的冗余参数；姿势可见性阈值收敛到 `hasVisiblePose`。
- `AppState` 的图库句柄统一走一次取锁；`TempFile` 改为 `Option<PathBuf>`；剪贴板打开逻辑收敛为一个 helper。
- 文档同步：架构分层与存储布局补 `img://`、`temp/viewer/`；修正 `img://` 状态码说明与 `temp` 缓存无上限的待决策项。

浏览器预览（Chromium 代理，724×712 CSS，mock 数据）：

- 查看器覆盖工作区，工具面板 262px、画布与 320px 侧栏无溢出；适应缩放 30%，滚轮 30%→40%，双击回到适应。
- 旋转点亮“未保存”；裁剪拖拽 306px 屏幕距离映射为 1,024×1,024 原图像素；调尺寸 512、重置、上下张与 Inspector/网格选择联动、侧栏开关、关闭均正常；无控制台错误。
- 该检查不代表 WebView2 的图片解码、剪贴板、全屏或保存性能。

## 未验证

- **主库真实窗口验收**：应用打开 `G:\Lilith Image Library`，检查树/计数/缩略图/查看器/pose/搜索/评分/标签/描述/编辑/AI/S7。
- **独立测试库验收**：应用切换到 `G:\Lilith Test Library`，确认 9 455 张、6 个文件夹、87 pose 与缩略图正常。
- **失败内容后续处理**：`G:\Lilith Image Library Failed` 的 247 条超过 1 亿像素的图片尚未缩图/重导入；`manifest.jsonl` 保留元数据。
- **真实集合迁移**：六个旧库 `saved_lists.dat` 全为空，只有单元测试覆盖。
- 真实 Tauri 窗口的单库流程：欢迎页创建/打开、启动自动打开（当前设置已指向主库）、设置页更换图库、关闭图库。
- 真实窗口文件夹树、面包屑、右键菜单、拖放、空文件夹与子树计数；导入“保留源目录”。
- 文件夹操作在磁盘占用、权限、目标已存在等异常下的失败与回滚。
- 真实 Tauri 窗口的 S1 导入、S2/S3 浏览与 S4 查看器全流程（含全屏、键盘、侧栏编辑）。
- 真实 WebView2 的原图/大图预览加载、EXIF 方向图显示与裁剪一致性、缩放平移帧率。
- 真实图库的保存 round-trip：旧原图删除、`img://` 与缩略图立即刷新、pHash/主色更新、embedding/pose 清除。
- 4K/8K 保存耗时与内存峰值；超过 5000 万像素的只读限制与拒绝提示。
- 真实 Windows 程序粘贴剪贴板图片；透明图与 EXIF 方向。
- 深分页中打开查看器后保存会刷新回第一页，上下张上下文会重置（已知限制）。
- 真实 AI 服务：health/ensure/embed/pose/describe 全链路、模型维度校验与失败文案。
- 真实窗口 AI 任务进度、取消、失败清单与手写描述保护；语义搜索分页与 Ctrl+A。
- 真实窗口找相似（五种 kind）与相似分组（pHash/embedding、阈值、截断与缺失提示、存为集合）。
- 相似度基准：视觉索引构建与失效重建、20 万 pHash 目标扫描、全库分组、主色扫描（基准代码已就绪，未运行）。
- 真实美术图上的 pHash 阈值（6/256）、embedding 余弦阈值（0.97）与主色距离阈值标定。
- 常驻视觉索引在 20/50 万行下的内存占用（按结构估算 20 万约 46 MB、50 万约 116 MB）。
- 真实窗口查看器姿势生成/叠加与详情侧栏 AI 描述。
- 文件夹移动、永久删除、清空回收站仍无进度事件与取消（S8/S9 前补齐）。
- 20 万行滚动、150 张缩略图从请求到可见、冷/热缓存与 WebView2 内存行为。
- release 构建的 exe 与 DLL 清单（S9）。

## 下一步

1. 体验整改继续 P1（浏览主界面：侧栏去重、主区头部、工具栏重排、右键菜单分组、新建集合、详情面板不挤缩略图、缩略图信息与显示模式、筛选零值、调试信息）；P0 已完成，批次划分与决策见 [迁移后体验整改计划](2026-10-24-体验整改计划.md)。
2. 用户执行 `npm run tauri dev`：应用设置已指向 `G:\Lilith Image Library`，先验收主库；再从设置页切换/打开 `G:\Lilith Test Library` 验收测试库。
3. 需要时处理失败内容：按 `G:\Lilith Image Library Failed\manifest.jsonl` 缩小尺寸后重新导入；当前只保证汇总与元数据完整。
4. 真实窗口验收 A–C 的单库外壳、文件夹树、导入、浏览、编辑、查看器、AI 任务、语义搜索与 S7 找相似/相似分组；待决策项见 [待办与待决策](todo.md)。
5. 需要在真实库上定阈值时，用复制出来的小样本跑相似度基准，不要对 `G:\Lilith Image Library` 直接调参或写入测试数据。
6. 规划文档已归档；新阶段按 [路线图](roadmap.md) 继续。

## 约束

- `G:\Lilith Image Library` 已由迁移器改造为 Artbank 库；旧 `.data` 已删除，不要再让旧程序写入该目录。
- 应用不读旧 `.data`；迁移器在仓库外 `research/library-migration`，不进入 `src-tauri` 与产品提交历史。
- 真实库只用于验收；测试、基准和演示仍使用临时目录或复制样本，避免写入真实库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏。

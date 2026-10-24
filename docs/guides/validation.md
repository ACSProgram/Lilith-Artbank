# 验证策略

## 允许与禁止

允许：

- `npm run typecheck`、`npm run build`；
- `cargo fmt --check`、`cargo check`、`cargo test`；
- `git diff --check`；
- 对真实图库目录做**只读**清点（`ls`、文件数、尺寸），不写入。

禁止：

- 输入注入：`SendInput`、鼠标/键盘模拟、`pyautogui`、注入式消息，或任何驱动真实应用窗口的自动化；
- **UI 调试**：启动 `npm run dev` / `npm run tauri dev`、浏览器预览或无头 Chromium，截图、抓 DOM、量布局、看渲染性能都算；
- **编译 release**：`cargo build --release` / `npm run tauri build`，也不为了“确认能打包”而全量编译；需要时由用户自己执行；
- 修改 `G:\Lilith Image Library` 下的任何文件（包括“测试性改名/删除/写入”）；
- 把真实图库路径写进测试或基准；测试一律用 `tempfile` 临时目录。

界面视觉、手感与真实窗口行为全部由用户在阶段验收时手工检查。代理只报告构建、类型、单测与基准的结论，并把验收步骤写清楚交给用户。

## 日常检查

```bash
npm run typecheck        # tsc --noEmit
npm run build            # tsc + vite build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
git diff --check
```

- 前端改动至少跑 typecheck；涉及构建配置时跑 build。
- Rust 改动至少跑 cargo check 与 cargo test。
- 导入改动跑 `cargo test`：包含混合格式入库、精确重复、合并策略、取消一致性与 pack 缺失；性能门槛单独跑 `perf_import_1000`。
- 浏览/查询改动跑 `cargo test`（键集分页、筛选、排序、搜索、计数、全选 id、集合）与 `perf_metadata_200k`；前端只跑 typecheck/build，网格与筛选的界面行为由用户手工验收。
- 元数据/回收站改动跑 `cargo test`：详情、评分/描述、标签增删改、文件夹移动回滚、软删除/恢复/永久删除/清空；文件操作测试只用临时图库。
- 查看器/几何编辑改动跑 `cargo test`：几何顺序（翻转→旋转→裁剪→缩放）、裁剪坐标、输出格式与 alpha、编辑上限；保存事务（新哈希发布、旧文件清理、元数据保留、`feature_blobs` 清空、冲突/无修改/回收站拒绝）；查看器源（直接服务、预览生成/复用/4096 上限、EXIF 方向、缺失原图）。前端另跑 typecheck/build，查看器布局、缩放、裁剪拖拽映射与上下张由用户手工验收。
- AI 改动跑 `cargo test --lib ai::`：客户端 mock（health/embedding/describe/pose 解析、5xx 重试、4xx 不重试）、feature pack 追加与包头校验、均值中心化与 top-K、批量任务与手写描述保护；另跑 `cargo clippy --all-targets` 与前端 typecheck/build。
- 大图预览、剪贴板、全屏、AI 服务与语义搜索只能在真实 Tauri 窗口由用户手工验收；代理不启动应用、不截图、不驱动界面。
- 提交前只核对一次 `git status` 与 diff，不反复刷状态。

## 数据库与文件

- schema/查询测试使用临时 SQLite 文件或内存库。
- 导入/编辑/清理测试使用小型确定性样本，重点验证：哈希、原子发布、不覆盖、取消边界、指针一致性。
- 不读取旧 `.data`；迁移脚本独立于应用。
- 200k 行基准会写入临时目录并在结束时删除；不要指向真实图库。

## 性能基准

默认被 `#[ignore]` 标记，按需运行：

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_metadata_200k
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_thumb_pack_read
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_thumb_bench_build
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --ignored --nocapture perf_import_1000
cargo test --manifest-path src-tauri/Cargo.toml --lib ai::embedding::perf_tests -- --ignored --nocapture
```

最后一项会生成约 614 MB 的临时 fp16 向量包，结束后删除；只用于 20 万条 × 1536 维扫描基准。

`perf_metadata_200k` 现在同时输出原始 SQL 与 S2 查询引擎（`query_page`、`filter_counts`、`query_ids`）的 p50/p95。

浏览器缩略图基准：

```bash
python tools/perf/thumb_bench/generate.py
node tools/perf/thumb_bench/server.mjs
# 打开 http://127.0.0.1:18767/?format=jpg 与 ?format=webp，看页面右下角结果
```

真实 WebView2 的缩略图基准通过开发构建的应用内入口执行，由用户手工点击：

```bash
npm run tauri dev
# 打开图库 → 工作区右上角“缩略图基准” → 生成样本 → 冷加载 / 热加载
```

浏览界面与导入界面同样只在真实 Tauri 窗口中做最终验收：

```bash
npm run tauri dev
# 打开图库 → 导入图片/拖入图片 → 关闭导入弹窗 → 网格/列表 → 筛选 → 选择 → 复制路径
```

S4 查看器的真实窗口验收：

```bash
npm run tauri dev
# 双击图片打开查看器 → 滚轮缩放/拖拽/双击/全屏 → ←/→ 或翻页 →
# 旋转/翻转/裁剪/调尺寸 → 选择输出格式与质量 → 保存
# 确认：原图文件替换、缩略图与查看器立即刷新、标签/评分/描述/集合保留、embedding/pose 清除
# 右键“复制图片” → 粘贴到画图/聊天工具；大图与 EXIF 方向图确认显示与裁剪一致
```

S6 AI 的真实窗口验收：

```bash
npm run tauri dev
# 设置 → AI 服务 → 检测/启动/加载模型 → 确认 health 能力与维度
# 选中图片或当前视图 → AI 任务 → embedding/pose/description 批量任务
# 确认进度、取消、失败清单、手写描述不被覆盖；语义搜索输入文本并翻页
# 查看器无 pose 时点击“生成姿势骨架”；详情侧栏点击“AI 生成”
```

浏览器预览的检查方式（不驱动真实窗口）：

```bash
npm run dev
# 打开 http://127.0.0.1:1420 → 创建演示图库 → 网格/列表/筛选/滚动
```

预览使用 4,000 行内存 mock 与 SVG data URL 缩略图，只用于检查布局、DOM 数量、筛选/排序重置、选择与右键菜单；不代表真实缩略图协议或性能。检查完关闭 dev server，不留下浏览器页面或后台进程。

代理不得用输入注入驱动该窗口；界面最终由用户手工确认。

发布产物依赖检查：

```bash
cargo build --manifest-path src-tauri/Cargo.toml
python tools/perf/pe_imports.py src-tauri/target/debug/lilith-artbank.exe
```

- 同一台机器、同一构建档、同一浏览器才有可比性；不要用跨机器墙钟下结论。
- 结果写入 [性能基准](performance.md) 的“实测记录”，注明日期、机器、构建档与浏览器版本。
- 新增热点路径必须补对应基准或说明为什么不影响门槛。

## 阶段验收

每个阶段的验收写入 `docs/planning/current-handoff.md`；通过后移入 `docs/planning/archive/`。不要把“计划实现”写成“已验收”。

性能阶段的硬门槛以 [性能基准](performance.md) 为准；未达门槛先解决，再继续堆功能。

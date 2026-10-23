# 验证策略

## 允许与禁止

允许：

- `npm run typecheck`、`npm run build`；
- `cargo fmt --check`、`cargo check`、`cargo test`；
- `git diff --check`；
- 用浏览器预览（`npm run dev`）或无头 Chromium 做静态渲染、DOM 结构、布局与性能检查；
- 对真实图库目录做**只读**清点（`ls`、文件数、尺寸），不写入。

禁止：

- 输入注入：`SendInput`、鼠标/键盘模拟、`pyautogui`、注入式消息，或任何驱动真实应用窗口的自动化；
- 未经用户明确安排执行 `cargo build --release` / `npm run tauri build`；
- 修改 `G:\Lilith Image Library` 下的任何文件（包括“测试性改名/删除/写入”）；
- 把真实图库路径写进测试或基准；测试一律用 `tempfile` 临时目录。

界面视觉、手感与真实窗口行为由用户在阶段验收时手工检查。代理只能报告构建、类型、单测、基准与无头渲染的结论。

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
```

浏览器缩略图基准：

```bash
python tools/perf/thumb_bench/generate.py
node tools/perf/thumb_bench/server.mjs
# 打开 http://127.0.0.1:18767/?format=jpg 与 ?format=webp，看页面右下角结果
```

- 同一台机器、同一构建档、同一浏览器才有可比性；不要用跨机器墙钟下结论。
- 结果写入 [性能基准](performance.md) 的“实测记录”，注明日期、机器、构建档与浏览器版本。
- 新增热点路径必须补对应基准或说明为什么不影响门槛。

## 阶段验收

每个阶段的验收写入 `docs/planning/current-handoff.md`；通过后移入 `docs/planning/archive/`。不要把“计划实现”写成“已验收”。

性能阶段的硬门槛以 [性能基准](performance.md) 为准；未达门槛先解决，再继续堆功能。

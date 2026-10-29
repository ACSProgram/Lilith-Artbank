# 日志与命令记录规范

本文件规定日志的等级、行格式、命令记录的取舍与轮换参数，是新增日志时的第一依据。
实现见 `src-tauri/src/app/trace.rs`（格式、运行标识、命令包装）与 `src-tauri/src/lib.rs`
（插件与轮换参数）。

本规范只覆盖**常规档**：事件驱动、常开、平时零噪声。周期性探针、前端诊断桥、取证通道
与看门狗属于诊断档，目前没有实现。

## 等级

| 来源 | 等级 |
| --- | --- |
| 发布构建 | `info` |
| 调试构建 | `debug` |
| `LILITH_ARTBANK_LOG_LEVEL` | 覆盖以上两者（`off` / `error` / `warn` / `info` / `debug` / `trace`） |

等级由 `log::set_max_level` 单独控制，调用点在 `lib.rs` 的应用 `setup` 里，且必须是
`setup` 的第一句。

两处容易踩的坑：

- **不要在 `tauri_plugin_log::Builder` 上调用 `.level()`**。插件初始化时会把 fern dispatch
  的等级当作全局 `max_level` 写进去，一旦固定成 `Info`，运行时再改 `max_level` 也放不出
  更低等级，调试构建会连带丢掉所有 `debug` 记录。
- **`set_max_level` 不能提前到 `run()` 开头**。日志插件在应用 `setup` 之前初始化，它自己
  会调用一次 `set_max_level`（取 fern dispatch 的默认值 `Trace`），早设的基线会被覆盖，
  `LILITH_ARTBANK_LOG_LEVEL` 也就失效。

## 行格式

```text
2026-10-28 17:13:30[run:3f9a12][INFO][command] command finished: name=trash_images elapsed_ms=12 outcome=ok
```

| 段 | 含义 |
| --- | --- |
| 时间 | 本地时区，秒级。插件默认格式用 UTC，因此这里自定义了 format |
| `[run:xxxxxx]` | 本次运行的短标识，同一次运行内每一行都相同 |
| `[LEVEL]` | `ERROR` / `WARN` / `INFO` / `DEBUG` / `TRACE` |
| `[target]` | Rust 模块路径；命令记录统一为 `command` |
| 消息 | 单行，`key=value` 字段，字段名稳定可解析 |

毫秒不放进前缀；需要精度的地方由消息里的 `elapsed_ms` 承担。

`Builder::timezone_strategy()` 会把 dispatch 的格式重置成插件默认格式，所以链式调用里
它必须排在 `.format()` **之前**，否则自定义格式（含 `[run:]`）会被静默丢弃。它的作用只剩
轮转文件名里的日期。

## 命令记录

100 个 Tauri 命令里 92 个走包装，分两档：

| 档 | 成功 | 失败 | 慢 |
| --- | --- | --- | --- |
| 写命令（58） | `started` + `finished`，带 `elapsed_ms` | `WARN command failed` | — |
| 读命令（34） | 不记录 | `WARN command failed` | 超过阈值记 `WARN command slow` |

- **写命令**指会改变图库状态或有外部副作用的命令：设置与图库开关、导入、文件夹、标签、
  评分与描述、移动与回收站、撤销与重做、集合、像素编辑、AI 服务、任务、MCP 审批，
  以及 `reveal_path` / `copy_text` / `copy_image` 这类外部副作用。
- **读命令**指不改变状态的查询。其中 `query_images`、`query_image_ids`、
  `get_image_states`、`get_viewer_source` 会随滚动与翻页反复触发，成功路径必须零落盘。
- 写命令成对记录的价值在崩溃现场：只有 `started` 没有 `finished`，说明进程死在这次操作里。
- 8 个不返回 `Result` 的命令（`get_settings`、`get_app_info`、`list_tasks` 等）没有失败
  路径，不包装。
- **不记录命令参数**。参数含路径、文件名、描述文本等用户内容；数量类信息由操作记录表
  承担。需要关联时用 `[run:]` 与时间戳对齐 `history` 表。

### 慢阈值

| 范围 | 阈值 |
| --- | --- |
| 其余读命令 | 500ms |
| `semantic_search`、`find_similar_images` | 3000ms |

500ms 相对元数据分页预算（50ms）已经是一个数量级的偏离；语义搜索与找相似在大库上天然
较慢，用同一阈值会变成每次搜索都告警。

### 新增一条命令记录时

1. 这个命令改变状态或有外部副作用吗？→ 写命令，用 `trace::write` / `trace::write_async`。
2. 只是查询？→ 读命令，用 `trace::read` / `trace::read_async`；确实天然很慢的，用
   `trace::read_async_with` 单独给阈值。
3. 命令体里的 `?` 保持在包装闭包内，提前返回不会漏记。

## 轮换

| 参数 | 值 | 位置 |
| --- | --- | --- |
| 单文件上限 | 4MB | `LOG_MAX_FILE_SIZE` |
| 保留归档 | 5 份 | `LOG_KEPT_FILES` |

- 活动文件 `logs/lilith-artbank.log`；轮转时改名为
  `logs/lilith-artbank_2026-10-28_17-13-30.log`（本地时区，秒级）。
- `KeepSome(5)` 保留 5 份归档，**活动文件不计入**，因此上限是 6 个文件、约 24MB。
- 插件只支持按大小轮换，没有按天轮换；需要按天要自写 rotator。
- `logs/mcp.log` 与 `logs/ai-server.log` 是两条独立旁路（stdio 桥与 Python 服务），
  不走本节的轮换。

## 常用筛选

```bash
grep '\[run:3f9a12\]' logs/lilith-artbank.log      # 单次运行
grep '\[command\]' logs/lilith-artbank.log         # 只看命令
grep -E '\[(WARN|ERROR)\]' logs/lilith-artbank.log # 只看异常
grep 'command failed' logs/lilith-artbank.log      # 只看失败
```

## 验证

- Rust：`cargo fmt --check`、`cargo check`、`cargo test`。
- 自检：`grep -c "trace::write\|trace::read" src-tauri/src/app/commands.rs` 应为 92；
  `grep -n "^\s*\.level(" src-tauri/src/lib.rs` 应无输出（只匹配调用，注释里的 `.level()` 不算）；
  `grep -n "timezone_strategy" -A 2 src-tauri/src/lib.rs` 里 `.format(` 必须排在它之后。
- 提交前：`git diff --check`。

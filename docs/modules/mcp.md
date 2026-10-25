# MCP 图库控制

支持 MCP 的 AI 客户端通过应用内服务读取当前图库。批次 1 只开放只读工具；写入、撤销/重做、导入、询问档确认队列属于后续批次，当前不存在这些工具。

传输与进程：

```text
MCP 客户端 (stdio) → lilith-artbank.exe --mcp-stdio
                     → 127.0.0.1:<port>/mcp（Bearer 口令）
                     → mcp::server → AppState → library/ai

MCP 客户端 (Streamable HTTP) → 应用内 HTTP 服务（同一地址）
```

- MCP 服务在应用进程内运行，工具直接调用 `AppState`，不经过 Tauri IPC，不另起图库写进程。
- `--mcp-stdio` 在 Tauri 初始化与 single-instance 插件之前处理；stdout 只写 MCP JSON-RPC，诊断写入 `logs/mcp.log`。
- HTTP 只绑定 `127.0.0.1`，每个请求都必须带 `Authorization: Bearer <token>`。
- 服务启动时生成随机口令，把配置端口与口令写入程序根目录的 `mcp-runtime.json`。stdio 小桥每次请求前读取它；应用重启后会自动换用新口令。
- 服务默认关闭；设置中开启后使用配置端口。设置保存后立即按新配置重启服务。

## 设置

设置 v6 增加：

```text
mcp { enabled: false, port: 18767, mode: "readOnly" | "ask" | "full" }
```

- `enabled` 默认 `false`；`port` 默认 `18767`，范围 1-65535。
- `mode` 默认 `ask`。批次 1 没有写工具，`library_info` 报告的实际 `mode` 固定为 `readOnly`。
- 端口被占用等启动失败只记录在 `get_mcp_status` 的错误里，不回滚设置。
- 设置页 MCP 分类显示开关、端口、运行状态与只读限制。

## 只读工具（8）

| 工具 | 输入 | 返回 |
| --- | --- | --- |
| `library_info` | — | 图库、计数、schema、特征数量、AI 配置、MCP 限制 |
| `folders_list` | `parentPath?`、`limit?` | 直接子文件夹、路径、直接/子树计数 |
| `tags_list` | `query?`、`limit?` | 标签 id、名称、图片数 |
| `collections_list` | — | 集合 id、名称、数量、顺序 |
| `search_images` | 结构化查询、`pageSize≤50`、`includeFacets?` | 总数、紧凑行、下一页游标 |
| `find_similar` | `mode: image/text`、目标图或文本、`kind`、阈值、`limit` | token、总数、前几条预览 |
| `images_get` | `ids≤50` | 详情、标签、描述、能力标记 |
| `image_sheet` | selector、`grid 1–5`、`resolution: thumb/detail` | 拼图图片、格子映射、`sheetId`、下一页 |

- `search_images` 始终返回 `total`；分面计数按需开启，2 字搜索与超大集合较慢。
- `find_similar` 复用应用唯一的相似结果槽：后一次相似或语义搜索会让前一个 token 失效。
- `images_get` 不返回像素数据；需要看图时用 `image_sheet`。
- `image_sheet` 的 `detail` 只允许 3×3 及以下，直接从原图解码；`thumb` 读缩略图包。两种清晰度都逐格解码，画完一格即释放，不把整盘图片同时留在内存里。

## 选择器

所有批量工具共享同一套 selector：

```json
{ "kind": "query", "query": { "...": "..." }, "excludeIds": [] }
{ "kind": "ids", "ids": [1, 2, 3] }
{ "kind": "sheet", "sheetId": "...", "cells": ["A1", "A2"], "excludeCells": ["A3"] }
{ "kind": "similarity", "token": "...", "offset": 0, "limit": 16 }
```

- `query`：结构化查询，游标字段与 `search_images` 相同。
- `ids`：明确 id，去重并保留输入顺序；不存在的 id 进入 `missingIds`。
- `sheet`：引用 `image_sheet` 返回的格子；`cells` 省略表示整盘。
- `similarity`：引用 `find_similar` 返回的 token，按 `offset` 分页。
- `excludeIds` 在所有 selector 上生效；`sheet` 另有 `excludeCells`。

## 拼图

- `grid` 1–5，默认 4（4×4 = 16 张）；5×5 用于粗筛。
- 画布长边约 1600px，格子等比 `contain`，不裁切，深灰背景。
- 每格左上角画 `A1`…`E5` 编号，使用内置 5×7 位图字体。
- 缺图、缩略图缺失或解码失败画占位，不阻塞整盘；映射里标 `thumbMissing`。
- `sheetId` 只保存格子与图片 id 映射，最多 128 份、2 小时过期；不缓存图片字节。
- 查询来源返回 `next` selector（含下一页游标）；相似来源返回新的 `offset`；`ids` 超出单盘时把剩余 id 放在 `next`。

## 安全与限制

- 只绑定环回地址；缺少或错误的 Bearer 口令返回 HTTP 401。
- 应用未运行、MCP 服务未开启、图库未打开时，工具返回可读错误。
- 批次 1 不提供永久删除、清空回收站、图片空间优化、像素编辑、批量 AI 任务、图库打开/关闭、设置修改。
- 不新建 headless 产品模式；便携包仍然只有一个 exe。
- 运行时文件是本地凭据，不要提交或分享；`.gitignore` 已排除 `mcp-runtime.json`。

## 代码落点

- `src-tauri/src/mcp/runtime.rs`：服务启停、端口、口令、`mcp-runtime.json`、Bearer 校验、stateless Streamable HTTP。
- `src-tauri/src/mcp/bridge.rs`：`--mcp-stdio` 逐行代理，维护协商版本。
- `src-tauri/src/mcp/server.rs`：rmcp 工具适配器与工具描述。
- `src-tauri/src/mcp/tools/read.rs`：只读工具核心逻辑。
- `src-tauri/src/mcp/selection.rs`：selector 解析、取消引用、拼图前取图。
- `src-tauri/src/mcp/sheet.rs`：拼图映射缓存与缩略图/原图读取。
- `src-tauri/src/mcp/audit.rs`：stdio 小桥的 `logs/mcp.log` 诊断。
- `src-tauri/src/imageops/contact_sheet.rs`：拼图布局、编号与 JPEG 编码。
- `src/app/SettingsDialog.tsx`：MCP 开关、端口与状态；`src/app/api.ts` 的 `get_mcp_status`。

## 验证

自动检查：设置 v5→v6 迁移与端口校验、Bearer 校验、stateless HTTP initialize/tools/list、selector、搜索总数与分页、拼图布局/缺图/缓存、相似结果 token 失效、临时图库只读工具。

真实客户端验收需要用户执行：

1. 启动应用并打开图库，在设置 → MCP 开启服务，确认状态为“服务运行中”。
2. 用支持 stdio 的客户端把 `--mcp-stdio` 加到本程序参数，调用 `library_info`。
3. 依次调用 `folders_list`、`search_images`、`image_sheet`，确认拼图有编号且映射与格子一致。
4. 调用 `find_similar` 后再调用一次，确认旧 token 失效的提示可读。
5. 关闭 MCP 服务，确认客户端收到明确连接错误；重启应用后确认 `mcp-runtime.json` 已更新。

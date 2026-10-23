# 浏览与检索

S2 已实现“20-50 万条元数据 + 当前页 150 张缩略图”的基础浏览路径：Rust 键集查询、前端虚拟网格/列表、基础筛选、选择与键盘。前端不持有全库数组，每次只请求一页。

## 已实现范围

统一查询 DTO（`src-tauri/src/library/query.rs` 与 `src/modules/library/types.ts`）：

```text
LibraryQuery {
  view: "all" | "trash",          // "collection" 保留给 S3，当前会拒绝
  collectionId?: number | null,   // 当前会拒绝
  search: string,                 // S2 只接受空串，搜索在 S3
  filters: {
    folderIds: number[],
    extensions: string[],
    scores: number[],             // 0-5
    tagIds: number[],
    tagMode: "any" | "all"
  },
  sort: { key: "importedAt" | "score" | "size", descending: boolean },
  cursor?: { sortKey: string, id: number } | null,
  limit?: number | null           // 默认 150，限制 1-300
}
```

- 筛选去重并校验：文件夹最多 256、扩展名最多 32、评分只允许 0-5、标签最多 64。
- 当前视图是 `all`；`trash` 查询已可用但还没有界面入口（S3）。
- 排序键都是整数列，游标 `sortKey` 用十进制字符串传递。
- 返回 `ImageRow` 只含列表需要的字段，不含 embedding/pose 本体：

```text
id, contentHash, fileExt, folderId, folderPath, filePath,
fileSize, width, height, format, hasAlpha, score,
importedAt, modifiedAt, thumbVersion, thumbWidth, thumbHeight,
tags: string[]
```

- `filePath` 是原图的绝对路径（`<folder>/<content_hash>.<ext>`），用于“复制文件路径”。
- 当前不保存原始文件名；网格显示尺寸、格式、大小与评分，列表额外显示标签、导入时间与文件夹。

## 命令契约

前端入口：`src/modules/library/api.ts`（`copy_text` 经 `src/shared/clipboard.ts`）。参数使用 camelCase。

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `query_images` | `{ query }` | `{ items, nextCursor }` |
| `query_image_ids` | `{ query }` | `number[]`（当前查询的全部 id，最多 100 万） |
| `get_filter_counts` | `{ query }` | `FilterCounts` |
| `get_folders` | — | `FolderInfo[]` |
| `get_tags` | `{ limit? }` | `TagInfo[]`（默认/最多 500，按活跃图片数降序） |
| `copy_text` | `{ text }` | `null`（Rust `arboard` 写系统剪贴板） |

`FilterCounts`：

```text
{
  total,
  folders:     [{ folderId, count }],
  extensions:  [{ value, count }],
  scores:      [{ value, count }],
  tags:        [{ tagId, count }]
}
```

- 每个分面计数会排除该分面自身的筛选条件。例如选中文件夹后，文件夹计数仍显示其他文件夹在其余筛选下的数量。
- `get_folders` 返回所有文件夹（含根目录）及其活跃图片总数；`get_tags` 只返回至少有一张活跃图片的标签。
- 计数在打开筛选面板或筛选变化后计算一次，不在滚动时计算。

## 查询与索引

- 排序统一追加 `id` 作为稳定次序：`ORDER BY <sort_col> <dir>, images.id <dir>`。
- 翻页使用行值比较，禁止深 `OFFSET`：

```sql
-- 降序
WHERE (images.score, images.id) < (?, ?)
-- 升序
WHERE (images.score, images.id) > (?, ?)
```

- 视图条件：`deleted_at IS NULL`（all）或 `deleted_at IS NOT NULL`（trash）。
- 文件夹筛选使用 `folder_id IN (...)`，当前是精确匹配，不自动包含子文件夹。
- 扩展名、评分、标签筛选都用参数化 `IN`；标签 `all` 为每个标签一个 `EXISTS`。
- 首屏、深分页、文件夹、标签都命中现有索引；20 万行实测见 [性能基准](../guides/performance.md)。
- 标签摘要在一页返回后按页内 id 批量查询，避免 N+1。

## 前端虚拟化

模块：

```text
src/modules/library/
  types.ts          边界 DTO
  api.ts            Tauri 命令入口 + 浏览器预览 mock
  mock.ts           浏览器预览用的确定性数据集与查询实现
  model.ts          查询构造、分页常量、格式化、选择范围、筛选键
  useImageQuery.ts  首屏/下一页、代次、查询与翻页状态类型、错误重试
  LibraryView.tsx   控制器：筛选、排序、视图、选择、键盘、右键菜单
  VirtualGrid.tsx   自适应列数虚拟网格
  VirtualList.tsx   固定行高虚拟列表
  virtual.ts        两个虚拟视图共用的滚动句柄类型
  FilterPanel.tsx   基础筛选面板
  LibraryThumbnail.tsx 占位、失败与重试
```

- 网格列数由容器宽度和设置里的 `ui.gridThumbPx` 决定；只挂载视口前后各一屏的行。
- 列表固定 44px 行高，32px 粘性表头；窄窗口隐藏“导入时间/文件夹”列。
- 缩略图只挂载当前可见 + 前后一屏的单元格；`<img decoding="async">` 指向 `thumb://`。失败显示占位，点击重试会给 URL 加 `retry` 参数绕过失败缓存。
- 浏览器预览（无 Tauri）用 4,000 行确定性 mock 数据与 SVG data URL 缩略图；只用于 UI/布局检查，不代表真实性能。
- 滚动接近已加载末尾时请求下一页；一页返回后追加，不整库载入。**限制**：已加载的页不会被回收，用户一直滚到全库末尾时前端会持有全部行；分页窗口回收留给 S9。

## 选择、键盘与菜单

- 单击选中；Ctrl/Cmd 单击切换；Shift 单击按当前已加载顺序区间选择。
- `Ctrl+A` 调 `query_image_ids` 取当前筛选的全部 id，包含尚未滚动加载的图片；状态栏显示总数。
- 方向键移动活动项（网格按列数上下移动），Shift+方向键扩展区间；PageUp/PageDown、Home/End 滚动。
- Enter 或双击调用查看器入口；查看器本身在 S4，当前只提示占位。
- Delete 在 S3 接入回收站，当前提示。
- 右键菜单：打开查看器（占位）、复制文件路径、选择全部匹配项。
- 复制路径走 Rust `copy_text`（`arboard`），浏览器预览回退到 clipboard API / `execCommand`。
- 拖出到资源管理器（Windows OLE）本轮评估后未在 S2 实现；复制图片也留到 S4。当前可用的回退是复制文件路径。

## 筛选面板

- 文件夹（复选）、扩展名、评分、标签（任一/全部），显示条件计数。
- 标签过多时面板内可搜索标签名；列表最多 500 个。
- “清除筛选”与筛选摘要行；“完成”只关闭面板，不撤销已应用的筛选。
- 搜索、时间/大小范围、描述/embedding/pose/颜色/pHash 筛选在 S3/S7。

## 请求竞态与刷新

- `useImageQuery` 为每次首屏请求递增代次；过期结果直接丢弃，不覆盖新视图。
- 翻页请求与首屏共享代次；筛选/排序/刷新变化后旧页不会追加到新结果。
- 首屏失败显示整页错误与重试；翻页失败保留已加载页，在列表底部提供重试。
- 排序变化会重新从第一页加载并清空滚动位置，但保留图片选择；筛选变化会清空选择。
- 导入结束会递增 `refreshToken`，重新查询第一页并刷新图库计数。
- 切换图库时 `LibraryView` 以图库 id 为 key 重新挂载，避免复用上一库的筛选与选择。

## 性能

20 万行合成库、4 个读连接、开发档的查询引擎实测：

| 路径 | p50 | p95 | 预算 |
| --- | --- | --- | --- |
| 首屏 150（导入时间降序） | 0.62 ms | 0.79 ms | ≤50 ms |
| 深分页 150（score/id 游标） | 1.08 ms | 1.24 ms | ≤50 ms |
| 文件夹 150 | 0.58 ms | 0.83 ms | ≤50 ms |
| 标签 150 | 2.91 ms | 3.16 ms | ≤50 ms |
| 筛选分面计数 | 27.36 ms | 40.32 ms | ≤100 ms |
| Ctrl+A 取 20 万 id | 17.61 ms | — | 未设硬门槛 |

浏览器预览（724×712 CSS，4,000 行 mock）验证了 DOM 有界：加载 150 行时同时挂载 12-24 个单元格，加载 300 行后约 18 个；滚动到底会继续加载下一页；筛选/排序切换后回到第一页且无旧行残留。真实 WebView2 的 20 万行滚动帧率、150 张缩略图加载与真实美术样本仍需手工验收。

## 尚未实现

- 文本搜索（FTS5 trigram）与搜索框；时间/大小、有描述/embedding/pose、颜色/pHash 筛选。
- 集合视图与集合运算；回收站界面、移入/恢复/清空。
- 列表列头点击排序、列隐藏；当前用工具栏排序。
- 拖出到资源管理器（Windows OLE）、复制图片到剪贴板（S4）。
- 原始文件名：当前不入库；如需按文件名识别，需要 schema 变更与导入记录。
- 已加载页的窗口回收；长时间滚动到 20 万末尾时前端内存会随已加载行增长。

# 浏览与检索

浏览的目标是“20-50 万条元数据 + 当前页 150 张缩略图”在滚动时保持稳定。前端不持有全库数组；Rust 每次返回一页，数据库用键集游标翻页。

## 页面查询

统一查询 DTO（Stage 2/3 实现）：

```text
LibraryQuery {
  view: "all" | "trash" | "collection",
  collectionId?: number,
  search: string,
  filters: {
    folderIds: number[],
    extensions: string[],
    scores: number[],
    tagIds: number[],
    tagMode: "any" | "all",
    importedAfter?: number,
    importedBefore?: number,
    minSize?: number,
    maxSize?: number,
    hasDescription?: boolean,
    hasEmbedding?: boolean,
    hasPose?: boolean,
    colorSimilarity?: { colors: ColorScheme, min: number, max: number },
    pHash?: { hash: string, minDistance: number, maxDistance: number }
  },
  sort: {
    key: "score" | "importedAt" | "size" | "pHash" | "colors" | "clip" | "pose",
    descending: boolean,
    targetImageId?: number
  },
  cursor?: { sortKey: string, id: number },
  limit: number
}
```

- 返回 `{ items, nextCursor, total? }`。`total` 只在需要时单独查询，列表滚动不做全量 count。
- 排序键与 id 组成游标；翻页条件为 `(sort_key, id) < (?, ?)`（降序）或反向。禁止深 `OFFSET`。
- 默认每页 150 条；前端可请求 100-300。
- 查询结果只含列表需要的字段（id、缩略图指针/版本、宽高、评分、标签摘要、描述片段等），不返回 embedding/pose 本体。

## 常用 SQL 形状

- 评分：`WHERE deleted_at IS NULL ORDER BY score DESC, id DESC`，命中 `(deleted_at, score DESC, id DESC)`。
- 时间/大小：对应复合索引。
- 文件夹：`folder_id = ?` 命中 `(folder_id, deleted_at, imported_at DESC, id DESC)`。
- 标签：`JOIN image_tags`；`any` 用 `IN` + `DISTINCT`，`all` 用分组计数或多次 `EXISTS`。
- 文本：FTS5 `MATCH`；`trigram` 对少于 3 个字符的查询回退 `LIKE '%x%'`（并限定当前视图/文件夹，避免全表）。
- 回收站：`deleted_at IS NOT NULL`；恢复只清空 `deleted_at`。
- 集合：`JOIN collection_items`，按 `position` 排序；集合只存 id 引用，图片编辑换内容哈希不影响集合。

## 筛选统计

筛选面板的计数（文件夹、扩展名、评分、标签）按需计算：

```sql
SELECT folder_id, count(*) FROM images
WHERE deleted_at IS NULL AND <当前筛选>
GROUP BY folder_id;
```

- 打开筛选面板时计算一次；筛选条件变化时重算；不在每次滚动时计算。
- 20 万行、20 个文件夹实测约 8.7ms；标签计数更贵，先显示缓存/加载态，必要时再引入统计表。
- 计数只用于辅助选择，不作为分页总数承诺。

## 前端

- 虚拟化网格：只挂载视口 + 前后一屏的单元格；列数由容器宽度与 `gridThumbPx` 决定。
- 缩略图：`<img>` 指向 `thumb://` 协议 URL，`decoding="async"`；进入视口前 1-2 屏预取；失败显示占位并可重试。
- 列表视图：列可排序/隐藏；行高固定，虚拟滚动。
- 选择：单击、Ctrl 多选、Shift 范围、Ctrl+A；选择集只存 id。右键菜单基于选择集。
- 拖动：内部拖动用于“加入集合/合并”；拖到资源管理器用 Windows OLE 文件拖出（Stage 2 评估，失败则提供“复制图片/复制路径”）。
- 键盘：Enter 打开查看器，Delete 移入回收站，Ctrl+C 复制，Ctrl+E 批量编辑，Ctrl+Z 撤销视图变更，Esc 关闭查看器/弹窗。
- 请求竞态：controller 使用请求代次；过期的页结果丢弃，不覆盖新视图。
- 错误只上报一次；滚动失败保留已有页并显示重试入口。

## 集合与撤销

- 集合是手工有序列表；支持新建、重命名、删除、拖拽排序、加入/移出当前选择。
- 集合运算（交集/并集/差集）：对两个集合的 id 集在 Rust 侧计算，结果作为新的临时视图或新集合；不修改源集合。
- 视图撤销栈保存“查询描述 + 结果 id 集引用”而不是全量数据；撤销只回退视图，不撤销元数据编辑。
- 原实现的快照栈会在每次筛选/排序时复制整页数组；新实现只存查询参数，内存与集合大小无关。

## 回收站

- 删除 = `deleted_at` 置为当前时间；文件不移动，立刻从正常视图消失。
- 恢复 = `deleted_at = NULL`。
- 清空/永久删除 = 在事务中删除行，提交后删除原图与缩略图指针；文件删除失败进入清理账本（Stage 8）。
- 原实现在卸载时物理移动/删除回收站，语义容易误解；新实现显式“移入回收站/清空回收站”。
- 删除前确认；清空回收站是不可逆操作，使用危险样式确认。

## 性能门槛

| 场景 | 目标 |
| --- | --- |
| 首屏 150 条元数据 | ≤50ms（实测 0.12ms） |
| 深分页 150 条 | ≤50ms（实测 0.53ms） |
| 按文件夹 150 条 | ≤50ms（实测 0.07ms） |
| 按标签 150 条 | ≤50ms（实测 1.95ms） |
| FTS 查询 | ≤50ms（实测 0.48ms） |
| 一页 150 张缩略图从请求到可见 | ≤1s（实测 Chromium 0.44-0.46s 冷加载） |
| 滚动 | 目标 60fps；不因缩略图解码阻塞主线程 |
| 前端内存 | 不随图库总行数线性增长 |

新增筛选/排序必须同时给出 SQL 与索引；没有索引的全表扫描只能用于显式触发的分析任务（如相似度），不能放在滚动路径上。

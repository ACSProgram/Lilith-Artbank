import type { SortSpec } from "./contracts/library";

export type ThemeMode = "light" | "dark" | "system";
/** 缩略图在方格里的填充方式：裁剪铺满 / 完整可见。 */
export type ThumbFit = "cover" | "contain";

/** 排序方案：全局共享，存设置，数量上限 50 是存储上限。 */
export interface SortProfile {
  id: string;
  name: string;
  sort: SortSpec[];
}

export interface UiSettings {
  theme: ThemeMode;
  gridThumbPx: number;
  thumbFit: ThumbFit;
  thumbCacheCount: number;
  /** 全局一套排序，叠加到全部图库视图（相似/语义结果除外）。 */
  sort: SortSpec[];
  sortProfiles: SortProfile[];
  viewerFilmstrip: boolean;
  /** 文件夹树拆成上下两个窗格；展开状态共享，滚动位置独立。 */
  folderSplit: boolean;
  /** 上窗格占文件夹树高度的比例，0.2–0.8。 */
  folderSplitRatio: number;
  /** 列表视图显示哪些列；缩略图列不在其中，永远显示。 */
  listColumns: ListColumn[];
  /** 相似分组合并时的保留规则；空数组表示按默认规则（评分 → 导入时间）。 */
  similarityKeepSort: SortSpec[];
}

/** 列表视图可隐藏的列；与 Rust `settings::LIST_COLUMNS` 一一对应。 */
export type ListColumn = "pixels" | "format" | "size" | "score" | "tags" | "importedAt" | "folder";

export const LIST_COLUMNS: ListColumn[] = [
  "pixels",
  "format",
  "size",
  "score",
  "tags",
  "importedAt",
  "folder",
];

import type { ImportOptimize } from "../modules/import/types";
import type { SortSpec } from "../modules/library/types";

export type ThemeMode = "light" | "dark" | "system";
/** 缩略图在方格里的填充方式：裁剪铺满 / 完整可见。 */
export type ThumbFit = "cover" | "contain";

export interface AiSettings {
  baseUrl: string;
  port: number;
  autoStart: boolean;
  /** 为空时只连接 baseUrl 上已经启动的服务。 */
  pythonPath: string | null;
  serverScript: string | null;
  /** 模型根目录。 */
  modelPath: string | null;
  /** CLIP 模型目录；相对 modelPath，也可以填绝对路径。 */
  clipModelPath: string | null;
  /** 服务启动后在后台导入 torch/transformers；关掉可省下约 420 MB 常驻内存。 */
  prewarmImports: boolean;
}

export interface PerformanceSettings {
  readerConnections: number;
  workerThreads: number | null;
}

/** 每个图库保留的操作记录步数。 */
export interface HistorySettings {
  maxSteps: number;
}

/**
 * 导入偏好：目前只有「导入时压缩」。
 *
 * 由导入对话框的窄命令维护，设置页提交整份设置时后端保持原值。
 */
export interface ImportSettings {
  /** `null` 表示关闭（默认）；打开时记住档位与输出格式。 */
  optimize: ImportOptimize | null;
}

export type McpMode = "readOnly" | "ask" | "full";

export interface McpSettings {
  enabled: boolean;
  port: number;
  mode: McpMode;
}

export interface McpStatus {
  enabled: boolean;
  running: boolean;
  port: number;
  mode: McpMode;
  runtimePath: string;
  error: string | null;
}

export type McpApprovalStatus =
  | "pending"
  | "running"
  | "completed"
  | "rejected"
  | "expired"
  | "failed";

/** AI 写入前的待确认请求；只在内存里，10 分钟不处理自动过期。 */
export interface McpApproval {
  id: string;
  createdAt: number;
  expiresAt: number;
  status: McpApprovalStatus;
  title: string;
  tool: string;
  preview: unknown;
  result: unknown;
  error: string | null;
  taskId: string | null;
}

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

export interface MaintenanceSettings {
  /** 空闲时自动回收缩略图包；关掉之后「立即回收」仍然可用。 */
  autoCompactThumbs: boolean;
}

export interface Settings {
  settingsVersion: number;
  libraryPath: string | null;
  ai: AiSettings;
  performance: PerformanceSettings;
  history: HistorySettings;
  import: ImportSettings;
  mcp: McpSettings;
  ui: UiSettings;
  maintenance: MaintenanceSettings;
}

export interface LibraryManifest {
  format: string;
  formatVersion: number;
  libraryId: string;
  name: string;
  createdAt: number;
}

export type LibraryInspection =
  | { kind: "library"; manifest: LibraryManifest }
  | { kind: "empty" }
  | { kind: "invalid"; reason: string };

export interface LibraryInfo {
  id: string;
  name: string;
  path: string;
  imageCount: number;
  trashCount: number;
  /** 临时文件夹图片数；关闭/切换图库或退出时清空。 */
  tempCount: number;
  schemaVersion: number;
  openedAt: number;
}

export interface LibraryCounts {
  imageCount: number;
  trashCount: number;
  tempCount: number;
}

export interface AppInfo {
  version: string;
  settingsPath: string;
  logPath: string;
  exePath: string;
}

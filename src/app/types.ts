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
}

export interface PerformanceSettings {
  readerConnections: number;
  workerThreads: number | null;
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
}

export interface Settings {
  settingsVersion: number;
  libraryPath: string | null;
  ai: AiSettings;
  performance: PerformanceSettings;
  ui: UiSettings;
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
  schemaVersion: number;
  openedAt: number;
}

export interface AppInfo {
  version: string;
  settingsPath: string;
  logPath: string;
}

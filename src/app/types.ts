export interface LibraryEntry {
  id: string;
  name: string;
  path: string;
  addedAt: number;
}

export interface AiSettings {
  baseUrl: string;
  executablePath: string | null;
  port: number;
  autoStart: boolean;
}

export interface PerformanceSettings {
  readerConnections: number;
  workerThreads: number | null;
  thumbnailCacheMb: number;
}

export interface UiSettings {
  theme: "dark" | "light";
  gridThumbPx: number;
}

export interface Settings {
  settingsVersion: number;
  libraries: LibraryEntry[];
  lastLibraryId: string | null;
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
}

export interface ThumbBenchInfo {
  count: number;
  thumbBytes: number;
  buildMs: number;
  readP50Ms: number;
  readP95Ms: number;
}

export interface ThumbReadStats {
  p50Ms: number;
  p95Ms: number;
  totalBytes: number;
}

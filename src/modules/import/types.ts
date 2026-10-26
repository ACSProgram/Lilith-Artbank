export type DuplicatePolicy = "skip" | "merge";
export type FolderMode = "flat" | "preserve";
export type ImportTarget = "library" | "temp";
export type ImportOptimizeTier = "extreme" | "high" | "medium";
export type ImportOptimizeFormat = "jpeg" | "webp";

export interface ImportOptimize {
  tier: ImportOptimizeTier;
  outputFormat: ImportOptimizeFormat;
}

export interface ImportFolder {
  id: number;
  path: string;
}

export type ImportPhase =
  | "scanning"
  | "processing"
  | "finishing"
  | "completed"
  | "cancelled"
  | "failed";

export interface ImportRequest {
  paths: string[];
  /** library 为普通图库；temp 为临时文件夹（不写操作记录，关闭时清空）。 */
  target: ImportTarget;
  folder: string;
  folderMode: FolderMode;
  tags: string[];
  score: number;
  duplicatePolicy: DuplicatePolicy;
  perceptualThreshold: number;
  /** null 表示不压缩，行为与以前一致。 */
  optimize: ImportOptimize | null;
}

export interface ImportProgress {
  jobId: string;
  phase: ImportPhase;
  target: ImportTarget;
  total: number;
  processed: number;
  imported: number;
  duplicates: number;
  merged: number;
  failed: number;
  similar: number;
  /** 勾选「导入时压缩」后实际被压缩替换的新增图片数。 */
  optimized: number;
  optimizedBytes: number;
  current: string;
  elapsedMs: number;
}

export interface ImportFailure {
  path: string;
  reason: string;
}

export interface ImportHint {
  imageId: number;
  similarTo: number;
  distance: number;
}

export interface ImportSnapshot extends ImportProgress {
  startedAt: number;
  finishedAt: number | null;
  failures: ImportFailure[];
  hints: ImportHint[];
  previewIds: number[];
}

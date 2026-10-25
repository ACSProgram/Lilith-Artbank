export type DuplicatePolicy = "skip" | "merge";
export type FolderMode = "flat" | "preserve";
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

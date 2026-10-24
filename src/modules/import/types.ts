export type DuplicatePolicy = "skip" | "merge";
export type FolderMode = "flat" | "preserve";

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

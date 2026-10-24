import type { LibraryQuery } from "../modules/library/types";

export type TaskKind =
  | "import"
  | "thumbnail"
  | "aiEmbedding"
  | "aiPose"
  | "aiDescription"
  | "similarityGroups"
  | "folderMove"
  | "permanentDelete"
  | "emptyTrash";

export type TaskLane = "write" | "ai" | "scan";
export type TaskPhase =
  | "queued"
  | "preparing"
  | "running"
  | "completed"
  | "cancelled"
  | "failed";

export type ConflictPolicy = "fail" | "queue" | "cancelActive";
export type BatchTaskKind = "thumbnail" | "aiEmbedding" | "aiPose" | "aiDescription";
export type TaskRequestKind = BatchTaskKind | "similarityGroups";

export interface TaskFailure {
  imageId: number | null;
  path: string;
  reason: string;
}

export interface TaskSnapshot {
  taskId: string;
  kind: TaskKind;
  label: string;
  scope: string;
  lane: TaskLane;
  phase: TaskPhase;
  total: number;
  completed: number;
  failed: number;
  skipped: number;
  message: string;
  currentImageId: number | null;
  startedAt: number;
  finishedAt: number | null;
  failures: TaskFailure[];
  cancellable: boolean;
}

export interface TaskSummary {
  taskId: string;
  kind: TaskKind;
  label: string;
  scope: string;
  lane: TaskLane;
  phase: TaskPhase;
}

/** 前端只传语义范围；id 集合由 Rust 侧解析。 */
export type TaskScope =
  | { type: "selected"; imageIds: number[] }
  | { type: "folder"; folderId: number; includeSubfolders: boolean }
  | { type: "collection"; collectionId: number }
  | { type: "view"; query: LibraryQuery }
  | { type: "library" };

export interface BatchTaskRequest {
  kind: TaskRequestKind;
  scope: TaskScope;
  overwrite: boolean;
  recalcVisuals: boolean;
  prompt: string | null;
  maxTokens: number | null;
  /** 相似分组：两种指标可单独或同时启用。 */
  groupPhash?: boolean;
  phashThreshold?: number | null;
  groupEmbedding?: boolean;
  embeddingThreshold?: number | null;
  conflict: ConflictPolicy;
}

export type TaskStartResult =
  | { status: "started"; task: TaskSnapshot }
  | { status: "conflict"; active: TaskSummary[] };

export const TASK_PHASE_LABELS: Record<TaskPhase, string> = {
  queued: "排队中",
  preparing: "准备中",
  running: "进行中",
  completed: "已完成",
  cancelled: "已取消",
  failed: "失败",
};

export function isTaskActive(task: TaskSnapshot): boolean {
  return task.phase !== "completed" && task.phase !== "cancelled" && task.phase !== "failed";
}

/** 已完成 + 失败 + 跳过 视为已处理，用于进度条百分比。 */
export function taskPercent(task: TaskSnapshot): number {
  if (task.total <= 0) return isTaskActive(task) ? 0 : 100;
  const done = task.completed + task.failed + task.skipped;
  return Math.min(100, Math.round((done / task.total) * 100));
}

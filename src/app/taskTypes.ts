import type { TaskScope } from "../shared/scope";

export type { TaskScope };

export type TaskKind =
  | "import"
  | "thumbnail"
  | "aiEmbedding"
  | "aiPose"
  | "aiDescription"
  | "similarityGroups"
  | "folderMove"
  | "permanentDelete"
  | "emptyTrash"
  | "undo"
  | "redo"
  | "releaseGarbage"
  | "optimizeEstimate"
  | "optimize"
  | "tempFeatures";

export type TaskLane = "write" | "ai" | "scan";
export type TaskPhase =
  | "queued"
  | "preparing"
  | "running"
  | "completed"
  | "cancelled"
  | "failed";

export type ConflictPolicy = "fail" | "queue";
export type BatchTaskKind = "thumbnail" | "aiEmbedding" | "aiPose" | "aiDescription";
export type TaskRequestKind =
  | BatchTaskKind
  | "similarityGroups"
  | "optimizeEstimate"
  | "optimize";

export type OptimizeTier = "extreme" | "high" | "medium";
export type OptimizeOutputFormat = "jpeg" | "webp";

export interface OptimizeOptions {
  tier: OptimizeTier;
  outputFormat: OptimizeOutputFormat;
  maxLongEdge: number | null;
  skipOptimized: boolean;
  regenerateThumbnails: boolean;
  recomputePhash: boolean;
  recomputeColors: boolean;
}

export interface OptimizeSkipBucket {
  count: number;
  bytes: number;
}

export interface OptimizeQualityCount {
  quality: number;
  count: number;
}

export interface OptimizeCurvePoint {
  /** 已处理的候选体积占比。 */
  coverage: number;
  savingsBytes: number;
  /** 按像素折算的累计耗时；与执行顺序一致。 */
  elapsedSeconds: number;
}

export interface OptimizeFolderEstimate {
  folderId: number;
  path: string;
  count: number;
  bytes: number;
  savingsBytes: number;
}

export interface OptimizeEstimate {
  taskId: string;
  createdAt: number;
  optionsFingerprint: string;
  /** 估算时候选集合的指纹；执行前比对，挡住“数量体积都对但换了一批图”。 */
  candidateFingerprint: string;
  /** 估算参数；接管任务时据此重建执行请求。 */
  options?: OptimizeOptions | null;
  /** 估算范围；接管任务时直接复用，不按当前浏览状态重拼。 */
  scope?: TaskScope | null;
  scopeLabel: string;
  sampleSize: number;
  libraryId: string;
  sampleCount: number;
  sampleBytes: number;
  coverage: number;
  /** 样本里 ≥ 32 MP 的张数；与候选同口径，用来说明尾部有没有被代表。 */
  sampleLargeCount: number;
  candidateLargeCount: number;
  /** 样本编码阶段的墙钟耗时（毫秒）。 */
  sampleElapsedMs: number;
  sampleBadPixelRatio: number;
  totalCount: number;
  totalBytes: number;
  candidateCount: number;
  candidateBytes: number;
  processCount: number;
  processBytes: number;
  skipCompact: OptimizeSkipBucket;
  skipOptimized: OptimizeSkipBucket;
  skipTransparent: OptimizeSkipBucket;
  skipTooLarge: OptimizeSkipBucket;
  /** 前置跳过：GIF。 */
  skipAnimated: OptimizeSkipBucket;
  skipColorProfile: OptimizeSkipBucket;
  skipDepth: OptimizeSkipBucket;
  skipQuality: OptimizeSkipBucket;
  skipNotSmaller: OptimizeSkipBucket;
  /** 候选内跳过：APNG、动画 WebP、多页 TIFF。 */
  skipMultiframe: OptimizeSkipBucket;
  /** 候选内跳过：数据库列与实际文件不符、解码后才发现的前置原因。 */
  skipLate: OptimizeSkipBucket;
  failedCount: number;
  failedBytes: number;
  savingsBytes: number;
  savingsMarginBytes: number;
  predictedBytes: number;
  estimatedSeconds: number;
  /** 预计耗时的误差区间（±秒）；覆盖 100% 时为 0。 */
  estimatedSecondsMargin: number;
  estimatedWorkSeconds: number;
  estimatedWriteSeconds: number;
  estimatedFinishSeconds: number;
  qualityCounts: OptimizeQualityCount[];
  curve: OptimizeCurvePoint[];
  folders: OptimizeFolderEstimate[];
  sampleFailures: string[];
}

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
  /** 后端按阶段耗时加权的百分比；null 表示按 completed / total 计算。 */
  progressPercent: number | null;
  message: string;
  currentImageId: number | null;
  /** 第一次真正开始运行的时间；排队与准备阶段为空。 */
  runStartedAt: number | null;
  /** 当前处理的相对路径；没有时回退为 #图片id。 */
  currentFile: string | null;
  /** 已提交批次的原图字节；只有压缩任务非 0。 */
  originalBytes: number;
  /** 已提交批次节省的字节；只有压缩任务非 0。 */
  savedBytes: number;
  ratePerSecond: number | null;
  etaSeconds: number | null;
  startedAt: number;
  finishedAt: number | null;
  failures: TaskFailure[];
  cancellable: boolean;
  /** 任务最终写入的操作记录 id；进行中通常为空。 */
  operationId: number | null;
}

export interface TaskSummary {
  taskId: string;
  kind: TaskKind;
  label: string;
  scope: string;
  lane: TaskLane;
  phase: TaskPhase;
}

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
  /** 空间优化估算参数。 */
  optimize?: OptimizeOptions | null;
  /** 执行压缩时提交的估算任务 id；估算任务不用。 */
  estimateId?: string | null;
  estimateSampleSize?: number | null;
  conflict: ConflictPolicy;
}

export interface BatchTaskPreviewCapability {
  model: string;
  dim: number;
}

export interface BatchTaskPreviewRequest {
  kind: BatchTaskKind;
  scope: TaskScope;
  overwrite: boolean;
  capability: BatchTaskPreviewCapability | null;
}

export interface BatchTaskPreview {
  total: number;
  /** 缺少模型维度或离线时为空。 */
  willProcess: number | null;
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

/** 进度条百分比：后端给了加权值就用它，否则按「已完成 + 失败 + 跳过 / 总数」计算。 */
export function taskPercent(task: TaskSnapshot): number {
  if (task.progressPercent !== null) {
    return Math.min(100, Math.max(0, task.progressPercent));
  }
  if (task.total <= 0) return isTaskActive(task) ? 0 : 100;
  const done = task.completed + task.failed + task.skipped;
  return Math.min(100, Math.round((done / task.total) * 100));
}

import type { ImportTarget } from "../modules/import/types";
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
  | "clearHistory"
  | "releaseGarbage"
  | "optimizeEstimate"
  | "optimize"
  | "tempFeatures"
  | "checkLibrary"
  | "export"
  | "orphanMove"
  | "packCompact";

export type TaskPhase =
  | "queued"
  | "preparing"
  | "running"
  | "cancelling"
  | "completed"
  | "cancelled"
  /** 空闲回收让位给用户；不是用户取消。 */
  | "yielded"
  | "failed"
  /** 做完能做的部分但有失败项：失败张数、未删旧文件数写在 message 里。 */
  | "partialFailed";

export type ConflictPolicy = "fail" | "queue";
export type BatchTaskKind =
  | "thumbnail"
  | "aiEmbedding"
  | "aiPose"
  | "aiDescription"
  | "checkFast"
  | "checkFull"
  | "export";
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

/** 候选内跳过的一类原因；`key` 与估算视图的图例一致。 */
export interface TaskSkipBucket {
  key: string;
  count: number;
}

export interface TaskSnapshot {
  taskId: string;
  kind: TaskKind;
  label: string;
  scope: string;
  phase: TaskPhase;
  total: number;
  completed: number;
  failed: number;
  skipped: number;
  /** 候选内跳过按原因细分；只有压缩任务非空，按张数降序。 */
  skipBreakdown: TaskSkipBucket[];
  /** 后端按阶段耗时加权的百分比；null 表示按「已结算 / 候选张数」计算（压缩固定如此）。 */
  progressPercent: number | null;
  message: string;
  currentImageId: number | null;
  /** 第一次真正开始运行的时间；排队与准备阶段为空。 */
  runStartedAt: number | null;
  /** 当前处理的相对路径；没有时回退为 #图片id。 */
  currentFile: string | null;
  /** 已处理字节：压缩任务是已提交批次的原图字节，导出任务是已复制字节；其余为 0。 */
  originalBytes: number;
  /** 已节省字节；只有压缩任务非 0。 */
  savedBytes: number;
  ratePerSecond: number | null;
  etaSeconds: number | null;
  startedAt: number;
  finishedAt: number | null;
  failures: TaskFailure[];
  cancellable: boolean;
  /** 任务最终写入的操作记录 id；进行中通常为空。 */
  operationId: number | null;
  /** 任务写入的新图片数；目前只有导入任务非 0。 */
  addedImages: number;
  /** 图库导入丢弃的临时区同内容行数；其他任务为 0。 */
  removedTempImages: number;
  /** 导入目标；只有导入任务有值。 */
  importTarget: ImportTarget | null;
}

export interface TaskSummary {
  taskId: string;
  kind: TaskKind;
  label: string;
  scope: string;
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
  /** 导出：目标目录与是否附带 `manifest.csv`。 */
  targetDir?: string | null;
  includeManifest?: boolean;
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
  cancelling: "取消中",
  completed: "已完成",
  cancelled: "已取消",
  yielded: "已让位",
  failed: "失败",
  partialFailed: "部分失败",
};

/** 取消中要等当前批次收尾；取消不回退已经做完的部分。 */
export const CANCEL_PENDING_NOTE = "等当前批次提交完成，已执行的部分不会撤销。";

export function isTaskActive(task: TaskSnapshot): boolean {
  return !isTerminalPhase(task.phase);
}

/** 终态：与 Rust 的 `TaskPhase::is_terminal` 一一对应，含「已让位」与「部分失败」。 */
export function isTerminalPhase(phase: TaskPhase): boolean {
  return (
    phase === "completed" ||
    phase === "cancelled" ||
    phase === "yielded" ||
    phase === "failed" ||
    phase === "partialFailed"
  );
}

/** 进度条百分比：后端给了加权值就用它，否则按「已完成 + 失败 + 跳过 / 总数」计算。 */
export function taskPercent(task: TaskSnapshot): number {
  if (task.progressPercent !== null) {
    return Math.min(100, Math.max(0, task.progressPercent));
  }
  return Math.min(100, Math.round(settledRatio(task) * 100));
}

/**
 * 已结算占比（0–1）：已替换 + 失败 + 已跳过 / 候选张数。
 *
 * 进度条、环图中心与曲线横轴都读它，压缩任务不再有第二套百分比。
 */
export function settledRatio(task: TaskSnapshot): number {
  if (task.total <= 0) return isTaskActive(task) ? 0 : 1;
  const done = task.completed + task.failed + task.skipped;
  return Math.min(1, Math.max(0, done / task.total));
}

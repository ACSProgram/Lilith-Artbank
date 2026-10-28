import type { OptimizeEstimate, TaskPhase } from "./taskTypes";

export interface RuntimeOptimizePoint {
  elapsedSeconds: number;
  savingsBytes: number;
  processed: number;
}

export interface RuntimePointOptions {
  minIntervalSeconds?: number;
  maxPoints?: number;
}

function finite(value: number | undefined | null): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

/**
 * 运行时采样：按时间间隔取点，只在节省字节或处理张数变化时记录。
 *
 * 超过上限时隔点丢弃一半，继续采样；最后一个点始终保留。
 */
export function appendRuntimePoint(
  points: RuntimeOptimizePoint[],
  sample: RuntimeOptimizePoint,
  options: RuntimePointOptions = {},
): RuntimeOptimizePoint[] {
  const minInterval = options.minIntervalSeconds ?? 2;
  const maxPoints = Math.max(2, options.maxPoints ?? 240);
  const elapsed = finite(sample.elapsedSeconds);
  if (elapsed < 0) return points;
  const last = points[points.length - 1];
  if (last) {
    if (elapsed < last.elapsedSeconds) return points;
    if (elapsed - last.elapsedSeconds < minInterval) return points;
    if (
      sample.savingsBytes === last.savingsBytes &&
      sample.processed === last.processed
    ) {
      return points;
    }
  }
  const next = [
    ...points,
    {
      elapsedSeconds: elapsed,
      savingsBytes: finite(sample.savingsBytes),
      processed: finite(sample.processed),
    },
  ];
  if (next.length <= maxPoints) return next;
  return next.filter(
    (_, index) => index % 2 === 0 || index === next.length - 1,
  );
}

export type OptimizeComparisonKey = "savings" | "duration" | "count";

export interface OptimizeComparisonRow {
  key: OptimizeComparisonKey;
  label: string;
  estimated: number;
  actual: number;
  /** 实际 − 预计；取消/失败时界面不展示这一列。 */
  delta: number;
}

export interface OptimizeComparisonInput {
  savedBytes?: number;
  elapsedSeconds?: number;
  completed?: number;
  status?: TaskPhase;
}

export interface OptimizeComparison {
  rows: OptimizeComparisonRow[];
  completed: boolean;
}

/** 预计与实际对照；取消或失败时保留实际部分，不写成“未达预计”的结论。 */
export function compareOptimizeEstimate(
  estimate: Pick<OptimizeEstimate, "savingsBytes" | "estimatedSeconds" | "processCount">,
  execution: OptimizeComparisonInput,
): OptimizeComparison {
  const rows: Array<Omit<OptimizeComparisonRow, "delta">> = [
    {
      key: "savings",
      label: "节省空间",
      estimated: finite(estimate.savingsBytes),
      actual: finite(execution.savedBytes),
    },
    {
      key: "duration",
      label: "耗时",
      estimated: finite(estimate.estimatedSeconds),
      actual: finite(execution.elapsedSeconds),
    },
    {
      key: "count",
      label: "处理张数",
      estimated: finite(estimate.processCount),
      actual: finite(execution.completed),
    },
  ];
  return {
    rows: rows.map((row) => ({ ...row, delta: row.actual - row.estimated })),
    completed: execution.status === "completed",
  };
}

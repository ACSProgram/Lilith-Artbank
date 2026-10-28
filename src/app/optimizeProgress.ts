import type { OptimizeCurvePoint, OptimizeEstimate, TaskPhase } from "./taskTypes";

export interface RuntimeOptimizePoint {
  elapsedSeconds: number;
  savingsBytes: number;
  /** 已结算张数（已替换 + 已跳过 + 失败）；曲线的横轴就是它。 */
  processed: number;
}

/** 实际曲线的一个点：与预估共用「已处理候选张数」横轴。 */
export interface OptimizeActualPoint {
  /** 已结算张数 / 候选张数，与顶部进度条同一个式子。 */
  coverage: number;
  savingsBytes: number;
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

/**
 * 把运行时采样折算到横轴：分母是候选张数，与预估曲线、顶部进度条同源。
 *
 * 覆盖率只增不减；同一覆盖率的重复点丢掉，避免实际线出现回退。
 */
export function actualCurvePoints(
  points: RuntimeOptimizePoint[],
  total: number,
): OptimizeActualPoint[] {
  if (!(total > 0)) return [];
  const ordered: OptimizeActualPoint[] = [];
  for (const point of points) {
    const coverage = Math.min(1, Math.max(0, finite(point.processed) / total));
    const previous = ordered[ordered.length - 1];
    if (previous && coverage <= previous.coverage) continue;
    ordered.push({ coverage, savingsBytes: finite(point.savingsBytes) });
  }
  return ordered;
}

/** 实际曲线在某个覆盖率处的累计节省；没走到那里返回 null（界面显示 —）。 */
export function actualAtCoverage(
  points: OptimizeActualPoint[],
  coverage: number,
): number | null {
  if (points.length === 0) return null;
  if (points[points.length - 1].coverage < coverage) return null;
  let savings = 0;
  for (const point of points) {
    if (point.coverage > coverage) break;
    savings = point.savingsBytes;
  }
  return savings;
}

/** 预估曲线在任意覆盖率处的累计节省与累计耗时；点之间线性插值。 */
export function curveAt(
  points: readonly OptimizeCurvePoint[],
  coverage: number,
): OptimizeCurvePoint {
  if (points.length === 0) return { coverage, savingsBytes: 0, elapsedSeconds: 0 };
  const target = Math.min(1, Math.max(0, coverage));
  let previous = points[0];
  for (const point of points) {
    if (point.coverage >= target) {
      const span = point.coverage - previous.coverage;
      const ratio = span > 0 ? (target - previous.coverage) / span : 1;
      return {
        coverage: target,
        savingsBytes:
          previous.savingsBytes + (point.savingsBytes - previous.savingsBytes) * ratio,
        elapsedSeconds:
          previous.elapsedSeconds + (point.elapsedSeconds - previous.elapsedSeconds) * ratio,
      };
    }
    previous = point;
  }
  return previous;
}

/** 前多少比例的图片算“前期”。 */
const EARLY_COVERAGE = 0.2;

/**
 * 前期收益偏小的一句话预告。
 *
 * 执行顺序按 bpp 从高到低，排在前面的多是「像素少、细节高」的小图，本身可省的绝对
 * 字节不多；同一批里还混着会被跳过的图。所以开头可能很久看不到空间变少。
 * 前 20% 的图片拿到的收益不到总收益的 10%（不到份额的一半）时，先把预期说在前面，
 * 省得盯着 0 以为卡住。
 */
export function earlySavingsHint(
  curve: readonly OptimizeCurvePoint[],
  totalSavings: number,
): string | null {
  if (curve.length < 2 || !(totalSavings > 0)) return null;
  const early = curveAt(curve, EARLY_COVERAGE).savingsBytes;
  if (!(early > 0)) {
    return "前 20% 的图片预计没有可压缩的空间，开始阶段进展可能较慢。";
  }
  const share = Math.min(1, early / totalSavings);
  if (share >= 0.1) return null;
  const label = share * 100 < 1 ? "不到 1%" : `${Math.round(share * 100)}%`;
  return `前 20% 的图片预计只贡献 ${label} 的节省，开始阶段进展可能较慢。`;
}

export type OptimizeComparisonKey = "savings" | "duration" | "count";

export interface OptimizeComparisonRow {
  key: OptimizeComparisonKey;
  label: string;
  estimated: number;
  actual: number;
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
  const rows: OptimizeComparisonRow[] = [
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
  return { rows, completed: execution.status === "completed" };
}

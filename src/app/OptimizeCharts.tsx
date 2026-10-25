import { Fragment } from "react";
import { formatBytes, formatDuration } from "../shared/format";
import type { OptimizeCurvePoint } from "./taskTypes";

/** 饼图图例的一项：处理、跳过，或某一类跳过原因。 */
export interface OptimizeShareSegment {
  key: string;
  label: string;
  count: number;
  bytes: number;
}

interface ShareChartProps {
  processed: OptimizeShareSegment;
  skipped: OptimizeShareSegment;
  /** 跳过的细分原因；调用方已按体积从大到小排好，并滤掉计数为 0 的项。 */
  breakdown: OptimizeShareSegment[];
}

const DONUT_SIZE = 132;
const DONUT_RADIUS = 46;
const DONUT_STROKE = 14;
const DONUT_CIRCUMFERENCE = 2 * Math.PI * DONUT_RADIUS;

/**
 * 处理 / 跳过占比环图 + 图例。
 *
 * 环图按张数分，图例同时给出张数与体积：张数对应任务进度，体积对应空间收益。
 */
export function OptimizeShareChart({ processed, skipped, breakdown }: ShareChartProps) {
  const total = processed.count + skipped.count;
  const share = total > 0 ? processed.count / total : 0;
  const arc = share * DONUT_CIRCUMFERENCE;
  const center = DONUT_SIZE / 2;
  const rows = [
    { ...processed, tone: "process" },
    { ...skipped, tone: "skip" },
    ...breakdown.map((item) => ({ ...item, tone: "sub" })),
  ];

  return (
    <div className="optimize-share">
      <svg
        className="optimize-donut"
        viewBox={`0 0 ${DONUT_SIZE} ${DONUT_SIZE}`}
        role="img"
        aria-label={`处理 ${processed.count.toLocaleString()} 张，跳过 ${skipped.count.toLocaleString()} 张`}
      >
        <circle
          className="optimize-donut-track"
          cx={center}
          cy={center}
          r={DONUT_RADIUS}
          strokeWidth={DONUT_STROKE}
        />
        {arc > 0 ? (
          <circle
            className="optimize-donut-arc"
            cx={center}
            cy={center}
            r={DONUT_RADIUS}
            strokeWidth={DONUT_STROKE}
            strokeDasharray={`${arc} ${DONUT_CIRCUMFERENCE - arc}`}
            transform={`rotate(-90 ${center} ${center})`}
          />
        ) : null}
        <text x={center} y={center - 2} textAnchor="middle" className="optimize-donut-value">
          {Math.round(share * 100)}%
        </text>
        <text x={center} y={center + 15} textAnchor="middle" className="optimize-donut-label">
          会处理
        </text>
      </svg>

      <div className="optimize-legend">
        {rows.map((row) => (
          <Fragment key={row.key}>
            <i className={`is-${row.tone}`} />
            <span className={row.tone === "sub" ? "is-sub" : undefined}>{row.label}</span>
            <strong>{row.count.toLocaleString()} 张</strong>
            <em>{formatBytes(row.bytes)}</em>
          </Fragment>
        ))}
      </div>
    </div>
  );
}

interface CurveChartProps {
  points: OptimizeCurvePoint[];
  totalSavings: number;
}

/** 里程碑：按体积从大到小执行，处理到这些比例时已拿到多少收益。 */
const MILESTONES = [0.2, 0.5, 1];

const CURVE_WIDTH = 480;
const CURVE_HEIGHT = 176;
const CURVE_LEFT = 64;
const CURVE_RIGHT = 16;
const CURVE_TOP = 20;
const CURVE_BOTTOM = 30;
const PLOT_WIDTH = CURVE_WIDTH - CURVE_LEFT - CURVE_RIGHT;
const PLOT_HEIGHT = CURVE_HEIGHT - CURVE_TOP - CURVE_BOTTOM;
const BASELINE = CURVE_TOP + PLOT_HEIGHT;

/** 曲线在任意覆盖率处的累计节省与累计耗时；点之间线性插值。 */
export function curveAt(points: OptimizeCurvePoint[], coverage: number): OptimizeCurvePoint {
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

/**
 * 累计节省曲线 + 里程碑表。
 *
 * 横轴是已处理的候选体积占比，纵轴是累计节省；耗时按像素折算，
 * 因此每个里程碑的耗时不是简单按体积线性分摊。
 */
export function OptimizeCurveChart({ points, totalSavings }: CurveChartProps) {
  const ordered = [...points].sort((left, right) => left.coverage - right.coverage);
  const maxSavings = Math.max(totalSavings, ...ordered.map((point) => point.savingsBytes), 1);
  const x = (coverage: number) =>
    CURVE_LEFT + Math.min(1, Math.max(0, coverage)) * PLOT_WIDTH;
  const y = (savings: number) =>
    BASELINE - (Math.min(Math.max(savings, 0), maxSavings) / maxSavings) * PLOT_HEIGHT;

  const line = ordered
    .map(
      (point, index) =>
        `${index === 0 ? "M" : "L"}${x(point.coverage).toFixed(1)} ${y(point.savingsBytes).toFixed(1)}`,
    )
    .join(" ");
  const area = `${line} L${x(1).toFixed(1)} ${BASELINE} L${x(0).toFixed(1)} ${BASELINE} Z`;

  return (
    <div className="optimize-curve-body">
      <svg
        className="optimize-curve-chart"
        viewBox={`0 0 ${CURVE_WIDTH} ${CURVE_HEIGHT}`}
        role="img"
        aria-label={`收益曲线：处理 20% 体积约省 ${formatBytes(
          curveAt(ordered, 0.2).savingsBytes,
        )}，处理 50% 约省 ${formatBytes(curveAt(ordered, 0.5).savingsBytes)}，全部处理约省 ${formatBytes(
          totalSavings,
        )}`}
      >
        {[0, 0.5, 1].map((fraction) => {
          const gridY = BASELINE - fraction * PLOT_HEIGHT;
          return (
            <g key={fraction}>
              <line
                className="optimize-chart-grid"
                x1={CURVE_LEFT}
                x2={CURVE_WIDTH - CURVE_RIGHT}
                y1={gridY}
                y2={gridY}
                strokeWidth={1}
                strokeDasharray={fraction === 0 ? undefined : "3 4"}
              />
              <text
                x={CURVE_LEFT - 8}
                y={gridY + 3.5}
                textAnchor="end"
                className="optimize-chart-axis"
              >
                {formatBytes(maxSavings * fraction)}
              </text>
            </g>
          );
        })}

        <path className="optimize-chart-area" d={area} />
        <path className="optimize-chart-line" d={line} strokeWidth={1.8} />

        {[0, 0.25, 0.5, 0.75, 1].map((tick) => (
          <text
            key={tick}
            x={x(tick)}
            y={BASELINE + 16}
            textAnchor="middle"
            className="optimize-chart-axis"
          >
            {Math.round(tick * 100)}%
          </text>
        ))}

        {MILESTONES.map((fraction) => {
          const point = curveAt(ordered, fraction);
          const markerX = x(fraction);
          const markerY = y(point.savingsBytes);
          const anchorEnd = fraction >= 1;
          return (
            <g key={fraction}>
              <circle
                className="optimize-chart-dot"
                cx={markerX}
                cy={markerY}
                r={3.4}
                strokeWidth={1.6}
              />
              <text
                x={anchorEnd ? markerX - 5 : markerX}
                y={markerY - 8}
                textAnchor={anchorEnd ? "end" : "middle"}
                className="optimize-chart-mark"
              >
                {formatBytes(point.savingsBytes)}
              </text>
            </g>
          );
        })}
      </svg>

      <div className="optimize-curve-side">
        <div className="optimize-curve-row is-head">
          <span>进度</span>
          <span>累计节省</span>
          <span>耗时</span>
        </div>
        {MILESTONES.map((fraction) => {
          const point = curveAt(ordered, fraction);
          return (
            <div key={fraction} className="optimize-curve-row">
              <span>{Math.round(fraction * 100)}%</span>
              <strong>{formatBytes(point.savingsBytes)}</strong>
              <em>{formatDuration(point.elapsedSeconds)}</em>
            </div>
          );
        })}
      </div>
    </div>
  );
}

import { Fragment } from "react";
import { formatBytes, formatDuration } from "../shared/format";
import type { OptimizeActualPoint } from "./optimizeProgress";
import { actualAtCoverage, curveAt } from "./optimizeProgress";
import type { OptimizeCurvePoint } from "./taskTypes";

/** 环图分段的语义色：处理 / 跳过 / 失败 / 未处理；`sub` 只出现在图例里。 */
export type OptimizeShareTone = "process" | "skip" | "fail" | "pending" | "sub";

/** 图例的一行：处理、跳过、失败、未处理，或某一类跳过原因。 */
export interface OptimizeShareRow {
  key: string;
  label: string;
  count: number;
  bytes: number;
}

export interface OptimizeShareSegment {
  key: string;
  label: string;
  count: number;
  /** 体积；执行中的实际计数没有逐段体积，传 null 时图例不显示这一列。 */
  bytes: number | null;
  tone: OptimizeShareTone;
}

interface ShareChartProps {
  /** 环图分段，按顺序画弧；总张数就是分母。 */
  segments: OptimizeShareSegment[];
  /** 只进图例、不画弧的细分项；调用方已滤掉 0 项并排好序（估算按体积、执行按张数）。 */
  breakdown?: OptimizeShareSegment[];
  /** 细分项插在哪一段之后；不给就排在图例末尾。 */
  breakdownAfter?: string;
  /** 中心百分比取哪一段（按 key）；给了 `centerValue` 时不用它。 */
  centerKey: string;
  centerLabel: string;
  /** 中心直接显示的占比（0–1）；执行时传 `settled / total`，与进度条同源。 */
  centerValue?: number;
}

const DONUT_SIZE = 132;
const DONUT_RADIUS = 46;
const DONUT_STROKE = 14;
const DONUT_CIRCUMFERENCE = 2 * Math.PI * DONUT_RADIUS;

/**
 * 分段环图 + 图例。
 *
 * 估算时是「处理 / 候选内跳过」，执行时换成「已替换 / 已跳过 / 失败 / 未处理」，
 * 中心的百分比与顶部进度条同源（执行时由调用方直接给值）。
 */
export function OptimizeShareChart({
  segments,
  breakdown = [],
  breakdownAfter,
  centerKey,
  centerLabel,
  centerValue,
}: ShareChartProps) {
  const total = segments.reduce((sum, segment) => sum + segment.count, 0);
  const center = DONUT_SIZE / 2;
  const main = segments.find((segment) => segment.key === centerKey);
  const share =
    centerValue != null
      ? Math.min(1, Math.max(0, centerValue))
      : main && total > 0
        ? main.count / total
        : 0;
  // 细分行紧跟在它所属的那一段后面，否则读起来像挂在最后一段（未处理）下面。
  const rows = breakdownAfter
    ? segments.flatMap((segment) =>
        segment.key === breakdownAfter ? [segment, ...breakdown] : [segment],
      )
    : [...segments, ...breakdown];
  const withBytes = rows.some((row) => row.bytes != null);

  let passed = 0;
  const arcs = segments.map((segment) => {
    const length = total > 0 ? (segment.count / total) * DONUT_CIRCUMFERENCE : 0;
    const arc = { segment, length, offset: passed };
    passed += length;
    return arc;
  });

  return (
    <div className="optimize-share">
      <svg
        className="optimize-donut"
        viewBox={`0 0 ${DONUT_SIZE} ${DONUT_SIZE}`}
        role="img"
        aria-label={segments
          .map((segment) => `${segment.label} ${segment.count.toLocaleString()} 张`)
          .join("，")}
      >
        <circle
          className="optimize-donut-track"
          cx={center}
          cy={center}
          r={DONUT_RADIUS}
          strokeWidth={DONUT_STROKE}
        />
        {arcs.map(({ segment, length, offset }) =>
          length <= 0 ? null : (
            <circle
              key={segment.key}
              className={`optimize-donut-arc is-${segment.tone}`}
              cx={center}
              cy={center}
              r={DONUT_RADIUS}
              strokeWidth={DONUT_STROKE}
              strokeDasharray={`${length} ${DONUT_CIRCUMFERENCE - length}`}
              strokeDashoffset={-offset}
              transform={`rotate(-90 ${center} ${center})`}
            />
          ),
        )}
        <text x={center} y={center - 2} textAnchor="middle" className="optimize-donut-value">
          {Math.round(share * 100)}%
        </text>
        <text x={center} y={center + 15} textAnchor="middle" className="optimize-donut-label">
          {centerLabel}
        </text>
      </svg>

      <div className={`optimize-legend${withBytes ? "" : " is-compact"}`}>
        {rows.map((row) => (
          <Fragment key={row.key}>
            <i className={`is-${row.tone}`} />
            <span className={row.tone === "sub" ? "is-sub" : undefined}>{row.label}</span>
            <strong>{row.count.toLocaleString()} 张</strong>
            {withBytes ? (
              <em>{row.bytes != null ? formatBytes(row.bytes) : "—"}</em>
            ) : null}
          </Fragment>
        ))}
      </div>
    </div>
  );
}

interface CurveChartProps {
  points: OptimizeCurvePoint[];
  /** 执行中的实际采样；与预估同一条「已处理张数」横轴。 */
  actual?: OptimizeActualPoint[];
  totalSavings: number;
}

/** 里程碑：处理到这些张数占比时已拿到多少收益；横轴刻度与它一一对齐。 */
const MILESTONES = [0.2, 0.4, 0.6, 0.8, 1];

const CURVE_WIDTH = 480;
const CURVE_HEIGHT = 176;
const CURVE_LEFT = 64;
const CURVE_RIGHT = 16;
const CURVE_TOP = 20;
const CURVE_BOTTOM = 30;
const PLOT_WIDTH = CURVE_WIDTH - CURVE_LEFT - CURVE_RIGHT;
const PLOT_HEIGHT = CURVE_HEIGHT - CURVE_TOP - CURVE_BOTTOM;
const BASELINE = CURVE_TOP + PLOT_HEIGHT;

/**
 * 累计节省曲线 + 里程碑表。
 *
 * 横轴是已处理的候选张数占比，纵轴是累计节省；预估是虚线，执行中的实际是实线，
 * 两条线同轴，可以直接看出实际是不是沿着预估走。
 */
export function OptimizeCurveChart({ points, actual = [], totalSavings }: CurveChartProps) {
  const ordered = [...points].sort((left, right) => left.coverage - right.coverage);
  const maxSavings = Math.max(
    totalSavings,
    ...ordered.map((point) => point.savingsBytes),
    ...actual.map((point) => point.savingsBytes),
    1,
  );
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
  const actualLine = actual
    .map(
      (point, index) =>
        `${index === 0 ? "M" : "L"}${x(point.coverage).toFixed(1)} ${y(point.savingsBytes).toFixed(1)}`,
    )
    .join(" ");
  const hasActual = actual.length > 1;

  return (
    <div className="optimize-curve-body">
      <svg
        className="optimize-curve-chart"
        viewBox={`0 0 ${CURVE_WIDTH} ${CURVE_HEIGHT}`}
        role="img"
        aria-label={`收益曲线：${MILESTONES.map(
          (fraction) =>
            `处理 ${Math.round(fraction * 100)}% 图片约省 ${formatBytes(
              curveAt(ordered, fraction).savingsBytes,
            )}`,
        ).join("，")}`}
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
        <path
          className="optimize-chart-line is-estimate"
          d={line}
          strokeWidth={1.6}
          strokeDasharray="4 3"
        />
        {hasActual ? (
          <path className="optimize-chart-line is-actual" d={actualLine} strokeWidth={2} />
        ) : null}

        {/* 刻度与里程碑点用同一套步长，否则点会落在标签之间，看起来像标错了。 */}
        {[0, ...MILESTONES].map((tick) => (
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
          // 五个里程碑挤在一条曲线上，文字只标首尾，其余看右侧表格。
          return (
            <g key={fraction}>
              <circle
                className="optimize-chart-dot"
                cx={markerX}
                cy={markerY}
                r={3.4}
                strokeWidth={1.6}
              />
              {fraction === MILESTONES[0] || anchorEnd ? (
                <text
                  x={anchorEnd ? markerX - 5 : markerX}
                  y={markerY - 8}
                  textAnchor={anchorEnd ? "end" : "middle"}
                  className="optimize-chart-mark"
                >
                  {formatBytes(point.savingsBytes)}
                </text>
              ) : null}
            </g>
          );
        })}
      </svg>

      <div className="optimize-curve-side">
        <div className="optimize-curve-row is-head">
          <span>进度</span>
          <span>累计节省</span>
          <span>耗时</span>
          {actual.length > 0 ? <span>实际</span> : null}
        </div>
        {MILESTONES.map((fraction) => {
          const point = curveAt(ordered, fraction);
          const reached = actualAtCoverage(actual, fraction);
          return (
            <div key={fraction} className="optimize-curve-row">
              <span>{Math.round(fraction * 100)}%</span>
              <strong>{formatBytes(point.savingsBytes)}</strong>
              <em>{formatDuration(point.elapsedSeconds)}</em>
              {actual.length > 0 ? (
                <em>{reached != null ? formatBytes(reached) : "—"}</em>
              ) : null}
            </div>
          );
        })}
      </div>
    </div>
  );
}

import type { ValueCount } from "./types";

/** 环图的一段；`key` 决定颜色，`value` 是张数。 */
export interface DonutSegment {
  key: string;
  label: string;
  value: number;
}

/** 合并后的“其他”段；环图里用灰段，不占分类色。 */
export const DONUT_OTHER_KEY = "other";
/** 评分环图里的“未评分”段；同样用灰段。 */
export const DONUT_UNSCORED_KEY = "unscored";

/** 分类环图：前 `limit` 项各占一段，其余并入“其他”；张数为 0 的项不画。 */
export function donutSegments(items: ValueCount[], limit: number): DonutSegment[] {
  if (items.length === 0) return [];
  const segments: DonutSegment[] = items.slice(0, limit).map((item) => ({
    key: item.value,
    label: item.value,
    value: item.count,
  }));
  const rest = items.slice(limit);
  if (rest.length > 0) {
    segments.push({
      key: DONUT_OTHER_KEY,
      label: "其他",
      value: rest.reduce((sum, item) => sum + item.count, 0),
    });
  }
  return segments.filter((segment) => segment.value > 0);
}

/** 评分为固定六档，5 → 1 再“未评分”；0 张的档保留，图例里显示 0。 */
export function scoreSegments(items: ValueCount[]): DonutSegment[] {
  const counts = new Map(items.map((item) => [Number(item.value), item.count]));
  const segments: DonutSegment[] = [5, 4, 3, 2, 1].map((score) => ({
    key: `score-${score}`,
    label: "★".repeat(score),
    value: counts.get(score) ?? 0,
  }));
  segments.push({
    key: DONUT_UNSCORED_KEY,
    label: "未评分",
    value: counts.get(0) ?? 0,
  });
  return segments;
}

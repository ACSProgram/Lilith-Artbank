import type { ValueCount } from "./types";

export interface StatBarItem {
  label: string;
  count: number;
}

export function topValueCounts(items: ValueCount[], limit: number): StatBarItem[] {
  if (items.length === 0) return [];
  const head = items.slice(0, limit).map((item) => ({
    label: item.value,
    count: item.count,
  }));
  if (items.length <= limit) return head;
  const rest = items.slice(limit);
  return [
    ...head,
    {
      label: `其他（${rest.length} 种）`,
      count: rest.reduce((sum, item) => sum + item.count, 0),
    },
  ];
}

export function scoreBars(items: ValueCount[]): StatBarItem[] {
  const counts = new Map(items.map((item) => [Number(item.value), item.count]));
  return [0, 1, 2, 3, 4, 5].map((score) => ({
    label: score === 0 ? "未评分" : `${"★".repeat(score)}`,
    count: counts.get(score) ?? 0,
  }));
}

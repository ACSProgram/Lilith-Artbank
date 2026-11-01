import type { TaskScope } from "./scope";

export type SimilarKind = "phash" | "color4" | "color8" | "embedding" | "pose";

/** 「语义特征」指标内部的三种口径；后两种用 SPP / MaxSim 重排。 */
export type FeatureMode = "overall" | "subject" | "composition";

export const FEATURE_MODE_LABELS: Record<FeatureMode, string> = {
  overall: "整体",
  subject: "偏重主体",
  composition: "偏重构图",
};

export const SIMILAR_LABELS: Record<SimilarKind, string> = {
  phash: "pHash",
  color4: "主色 4 色",
  color8: "主色 8 色",
  embedding: "语义特征",
  pose: "姿势",
};

/** 平均参考图最多合并的图片数，与 Rust `library::similarity::MAX_REFERENCE_IDS` 一致。 */
export const MAX_SIMILAR_REFERENCE = 100;

export interface FindSimilarConfig {
  kind: SimilarKind;
  /** 仅 kind 为 embedding 时有效；其它指标统一用 overall。 */
  mode: FeatureMode;
  targetIds: number[];
  /** pHash 传距离上限（256 表示不限），其它指标传分数阈值；null 表示不限制。 */
  threshold: number | null;
  limit: number;
  scope: TaskScope;
  scopeLabel: string;
}

export function similarKindLabel(
  kind: SimilarKind,
  average: boolean,
  mode: FeatureMode = "overall",
): string {
  const base = (() => {
    if (!average) return SIMILAR_LABELS[kind];
    switch (kind) {
      case "color4":
        return "主色 4 色（平均）";
      case "color8":
        return "主色 8 色（平均）";
      case "embedding":
        return "语义特征（平均）";
      default:
        return SIMILAR_LABELS[kind];
    }
  })();
  if (kind === "embedding" && mode !== "overall") {
    return `${base} · ${FEATURE_MODE_LABELS[mode]}`;
  }
  return base;
}

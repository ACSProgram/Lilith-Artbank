import type { TaskScope } from "./scope";

export type SimilarKind = "phash" | "color4" | "color8" | "embedding" | "pose";

export const SIMILAR_LABELS: Record<SimilarKind, string> = {
  phash: "pHash",
  color4: "主色 4 色",
  color8: "主色 8 色",
  embedding: "语义 embedding",
  pose: "姿势",
};

/** 平均参考图最多合并的图片数，与 Rust `library::similarity::MAX_REFERENCE_IDS` 一致。 */
export const MAX_SIMILAR_REFERENCE = 100;

export interface FindSimilarConfig {
  kind: SimilarKind;
  targetIds: number[];
  /** pHash 传距离上限（256 表示不限），其它指标传分数阈值；null 表示不限制。 */
  threshold: number | null;
  limit: number;
  scope: TaskScope;
  scopeLabel: string;
}

export function similarKindLabel(kind: SimilarKind, average: boolean): string {
  if (!average) return SIMILAR_LABELS[kind];
  switch (kind) {
    case "color4":
      return "主色 4 色（平均）";
    case "color8":
      return "主色 8 色（平均）";
    case "embedding":
      return "语义（平均）";
    default:
      return SIMILAR_LABELS[kind];
  }
}

export type SimilarKind = "phash" | "color4" | "color8" | "embedding" | "pose";

export const SIMILAR_LABELS: Record<SimilarKind, string> = {
  phash: "pHash",
  color4: "主色 4 色",
  color8: "主色 8 色",
  embedding: "语义 embedding",
  pose: "姿势",
};

export type SimilarKind = "phash" | "color4" | "color8" | "embedding" | "pose";

export type SimilarGroupKind = "phash" | "embedding";

export const SIMILAR_LABELS: Record<SimilarKind, string> = {
  phash: "pHash",
  color4: "主色 4 色",
  color8: "主色 8 色",
  embedding: "语义 embedding",
  pose: "姿势",
};

export const SIMILAR_GROUP_LABELS: Record<SimilarGroupKind, string> = {
  phash: "pHash 相似组",
  embedding: "embedding 复核组",
};

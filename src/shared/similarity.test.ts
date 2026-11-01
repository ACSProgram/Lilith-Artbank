import test from "node:test";
import assert from "node:assert/strict";
import { FEATURE_MODE_LABELS, SIMILAR_LABELS, similarKindLabel } from "./similarity.ts";

test("similar_kind_label_uses_mode_for_semantic_kind", () => {
  assert.equal(similarKindLabel("embedding", false), "语义特征");
  assert.equal(
    similarKindLabel("embedding", false, "subject"),
    `语义特征 · ${FEATURE_MODE_LABELS.subject}`,
  );
  assert.equal(
    similarKindLabel("embedding", false, "composition"),
    `语义特征 · ${FEATURE_MODE_LABELS.composition}`,
  );
  // 平均参考只有「整体」一种口径，面板会强制回退，标题里不该带模式后缀。
  assert.equal(similarKindLabel("embedding", true, "overall"), "语义特征（平均）");
});

test("similar_kind_label_ignores_mode_for_other_kinds", () => {
  for (const kind of ["phash", "color4", "color8", "pose"] as const) {
    assert.equal(similarKindLabel(kind, false, "subject"), SIMILAR_LABELS[kind]);
  }
});

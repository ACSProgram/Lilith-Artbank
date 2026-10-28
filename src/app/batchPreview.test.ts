import test from "node:test";
import assert from "node:assert/strict";
import {
  batchPreviewKey,
  canStartBatchTask,
  createBatchPreviewCache,
  previewSummary,
  type BatchPreviewKeyInput,
} from "./batchPreview.ts";
import { emptyFilters } from "../modules/library/model.ts";
import type { LibraryQuery } from "../modules/library/types.ts";
import type { TaskScope } from "../shared/scope.ts";

const query: LibraryQuery = {
  view: "all",
  search: "",
  searchScope: "view",
  filters: emptyFilters(),
  sort: [],
};

function input(overrides: Partial<BatchPreviewKeyInput> = {}): BatchPreviewKeyInput {
  return {
    libraryId: "lib",
    kind: "thumbnail",
    scope: { type: "library" },
    overwrite: false,
    capability: null,
    dataVersion: 1,
    ...overrides,
  };
}

test("预览键随类型、跳过、范围、模型、数据版本与图库变化", () => {
  const base = batchPreviewKey(input());
  assert.notEqual(batchPreviewKey(input({ kind: "aiPose" })), base);
  assert.notEqual(batchPreviewKey(input({ overwrite: true })), base);
  assert.notEqual(batchPreviewKey(input({ scope: { type: "temp" } })), base);
  assert.notEqual(batchPreviewKey(input({ dataVersion: 2 })), base);
  assert.notEqual(batchPreviewKey(input({ libraryId: "other" })), base);
  assert.notEqual(
    batchPreviewKey(
      input({ capability: { model: "siglip", dim: 768 } }),
    ),
    base,
  );
  assert.notEqual(
    batchPreviewKey(
      input({
        kind: "aiEmbedding",
        capability: { model: "siglip", dim: 768 },
      }),
    ),
    batchPreviewKey(
      input({
        kind: "aiEmbedding",
        capability: { model: "siglip", dim: 1024 },
      }),
    ),
  );
});

test("选择集按长度与内容区分，数组副本得到同键", () => {
  const ids = [3, 1, 2];
  const first = batchPreviewKey(input({ scope: { type: "selected", imageIds: ids } }));
  const copy = batchPreviewKey(input({ scope: { type: "selected", imageIds: [...ids] } }));
  assert.equal(first, copy);
  assert.notEqual(
    first,
    batchPreviewKey(input({ scope: { type: "selected", imageIds: [2, 3, 1] } })),
  );
  assert.notEqual(
    first,
    batchPreviewKey(input({ scope: { type: "selected", imageIds: [3, 1] } })),
  );
});

test("视图键忽略排序与分页", () => {
  const scope = (value: LibraryQuery): TaskScope => ({ type: "view", query: value });
  const base = batchPreviewKey(input({ scope: scope(query) }));
  const reordered = batchPreviewKey(
    input({
      scope: scope({
        ...query,
        sort: [{ key: "score", descending: false }],
        limit: 300,
        pageToken: "abc",
      }),
    }),
  );
  assert.equal(base, reordered);
});

test("缓存按容量淘汰最久未用", () => {
  const cache = createBatchPreviewCache<number>(2);
  cache.set("a", 1);
  cache.set("b", 2);
  assert.equal(cache.get("a"), 1);
  cache.set("c", 3);
  assert.equal(cache.get("b"), undefined);
  assert.equal(cache.get("a"), 1);
  assert.equal(cache.get("c"), 3);
  assert.equal(cache.size, 2);
  cache.clear();
  assert.equal(cache.size, 0);
});

test("开始条件按顺序给理由", () => {
  const ready = {
    kindEnabled: true,
    scopeEnabled: true,
    embeddingDimMissing: false,
    previewLoading: false,
    willProcess: 12,
  };
  assert.deepEqual(canStartBatchTask(ready), { enabled: true, reason: null });
  assert.equal(
    canStartBatchTask({ ...ready, kindEnabled: false }).reason,
    "当前任务类型不可用",
  );
  assert.equal(
    canStartBatchTask({ ...ready, scopeEnabled: false }).reason,
    "当前范围不可用",
  );
  assert.equal(
    canStartBatchTask({ ...ready, embeddingDimMissing: true }).reason,
    "AI 服务未返回模型维度",
  );
  assert.equal(
    canStartBatchTask({ ...ready, previewLoading: true }).reason,
    "正在计算待处理数量",
  );
  assert.equal(
    canStartBatchTask({ ...ready, willProcess: 0 }).reason,
    "没有需要处理的图片",
  );
  assert.equal(
    canStartBatchTask({ ...ready, previewFailed: true, willProcess: null }).enabled,
    true,
  );
});

test("预览文案覆盖正常、加载、离线、缺维度、失败与零值", () => {
  const base = {
    total: 12_000,
    willProcess: 1_200,
    loading: false,
    offline: false,
    dimMissing: false,
    failed: false,
  };
  assert.equal(
    previewSummary(base),
    "范围 12,000 张 · 将处理 1,200 张",
  );
  assert.equal(
    previewSummary({ ...base, loading: true, willProcess: null }),
    "范围 12,000 张 · 将处理 —",
  );
  assert.equal(
    previewSummary({ ...base, offline: true, willProcess: null }),
    "范围 12,000 张 · 连接 AI 服务后计算",
  );
  assert.equal(
    previewSummary({ ...base, dimMissing: true, willProcess: null }),
    "范围 12,000 张 · AI 服务未返回模型维度",
  );
  assert.equal(
    previewSummary({ ...base, failed: true, willProcess: null }),
    "范围 12,000 张 · 暂时无法计算待处理数量",
  );
  assert.equal(
    previewSummary({ ...base, willProcess: 0 }),
    "范围 12,000 张 · 将处理 0 张 · 没有需要处理的图片",
  );
});

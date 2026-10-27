import test from "node:test";
import assert from "node:assert/strict";
import {
  buildQuery,
  defaultBrowseState,
  queryIdentityKey,
  querySortKey,
  type BrowseState,
} from "./model.ts";
import type { LibraryFilters } from "./types.ts";
import {
  combinePlans,
  dataChangeForMcp,
  dataChangeForTaskKind,
  needsCursorRefresh,
  planDataChange,
  selectionAfterChange,
  touchesTags,
  type DataChange,
} from "./dataChange.ts";

function query(
  overrides: Omit<Partial<BrowseState>, "filters"> & { filters?: Partial<LibraryFilters> } = {},
) {
  const base = defaultBrowseState();
  return buildQuery({
    ...base,
    ...overrides,
    filters: { ...base.filters, ...(overrides.filters ?? {}) },
  });
}

const context = { hasMore: false };

function change(overrides: Partial<DataChange> = {}): DataChange {
  return {
    kind: "metadata",
    source: "local",
    similarity: "keep",
    ...overrides,
  };
}

test("评分不受当前查询影响时只 patch 已加载行", () => {
  const plan = planDataChange(
    change({ fields: ["score"], ids: [1], score: 5 }),
    query(),
    context,
  );
  assert.equal(plan.list, "patch");
  assert.equal(plan.selection, "keep");
});

test("评分筛选不接受新值时移除选择并刷新", () => {
  const filters = { scores: [3] };
  const current = query({ filters });
  const plan = planDataChange(
    change({ fields: ["score"], ids: [1], score: 5 }),
    current,
    context,
  );
  assert.equal(plan.list, "refresh");
  assert.equal(plan.selection, "prune");
  const rated = change({ fields: ["score"], ids: [1], score: 5 });
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), rated, current, [])], [2]);
});

test("标签添加保留选择；批量标签写入因缺少行数据而刷新", () => {
  const current = query({ filters: { tagIds: [7] } });
  const single = change({
    fields: ["tags"],
    tagAction: "add",
    ids: [1],
    rowPatches: [{ id: 1, update: { tags: ["人物"] } }],
  });
  assert.equal(planDataChange(single, current, context).selection, "keep");
  assert.deepEqual([...selectionAfterChange(new Set([1]), single, current, [])], [1]);

  const batch = change({ fields: ["tags"], tagAction: "add", ids: [1] });
  assert.equal(planDataChange(batch, query(), context).list, "refresh");
});

test("标签移除在当前标签筛选下剔除受影响的行", () => {
  const current = query({ filters: { tagIds: [7] } });
  const remove = change({ fields: ["tags"], tagAction: "remove", ids: [1] });
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), remove, current, [])], [2]);
});

test("描述变化按存在性筛选剔除选择", () => {
  const missing = query({ filters: { description: "missing" } });
  const filled = change({
    fields: ["description"],
    ids: [1],
    descriptionEmpty: false,
  });
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), filled, missing, [])], [2]);

  const has = query({ filters: { description: "has" } });
  const cleared = change({ fields: ["description"], ids: [1], descriptionEmpty: true });
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), cleared, has, [])], [2]);
});

test("成员变化：本地精确剔除，外部范围变化清空选择", () => {
  const local = change({ kind: "membership", source: "local", ids: [2] });
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), local, query(), [])], [1]);
  assert.equal(planDataChange(local, query(), context).selection, "prune");

  const external = change({ kind: "membership", source: "task" });
  assert.equal(planDataChange(external, query(), context).selection, "clear");
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), external, query(), [])], []);

  const imported = change({
    kind: "membership",
    source: "import",
    clearSelection: true,
    similarity: "clear",
  });
  assert.equal(planDataChange(imported, query(), context).similarity, "clear");
});

test("移动图片离开当前文件夹时剔除选择", () => {
  const current = query({ filters: { folderIds: [2] } });
  const move = change({
    kind: "folder",
    fields: ["folder"],
    ids: [1],
    targetFolderPath: "风景",
  });
  assert.deepEqual(
    [...selectionAfterChange(new Set([1, 2]), move, current, ["人物"])],
    [2],
  );
  assert.equal(planDataChange(move, current, context).list, "refresh");

  const inside = { ...move, targetFolderPath: "人物/线稿" };
  assert.deepEqual(
    [...selectionAfterChange(new Set([1]), inside, current, ["人物"])],
    [1],
  );
});

test("直属模式下移动到子文件夹即离开结果，移动到本文件夹保留", () => {
  const current = query({ filters: { folderIds: [2], folderDirect: true } });
  const toChild = change({
    kind: "folder",
    fields: ["folder"],
    ids: [1],
    targetFolderPath: "人物/线稿",
  });
  assert.deepEqual(
    [...selectionAfterChange(new Set([1, 2]), toChild, current, ["人物"])],
    [2],
  );
  const toSelf = { ...toChild, targetFolderPath: "人物" };
  assert.deepEqual(
    [...selectionAfterChange(new Set([1]), toSelf, current, ["人物"])],
    [1],
  );
});

test("集合视图移出成员时刷新并剔除选择；其它视图保持", () => {
  const inside = query({ view: "collection", collectionId: 3 });
  const remove = change({ kind: "collection", ids: [1] });
  assert.equal(planDataChange(remove, inside, context).list, "refresh");
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), remove, inside, [])], [2]);

  const outside = query();
  assert.equal(planDataChange(remove, outside, context).list, "none");
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), remove, outside, [])], [1, 2]);
});

test("特征写入只在缺少特征的筛选下改变成员关系", () => {
  const missing = query({ filters: { embedding: "missing" } });
  const features = change({ kind: "features", feature: "embedding", ids: [1] });
  assert.equal(planDataChange(features, missing, context).list, "refresh");
  assert.deepEqual([...selectionAfterChange(new Set([1, 2]), features, missing, [])], [2]);

  const has = query({ filters: { embedding: "has" } });
  assert.equal(planDataChange(features, has, context).list, "none");
  assert.deepEqual([...selectionAfterChange(new Set([1]), features, has, [])], [1]);
});

test("多级排序的 id 游标在还有下一页时必须刷新", () => {
  const multi = query({
    sort: [
      { key: "score", descending: true },
      { key: "importedAt", descending: true },
    ],
  });
  assert.equal(needsCursorRefresh(multi, true), true);
  const plan = planDataChange(change({ fields: ["score"], ids: [1], score: 1 }), multi, {
    hasMore: true,
  });
  assert.equal(plan.list, "refresh");
});

test("排序变化不改变结果集身份，只走保留旧行的刷新", () => {
  const byScore = query({ sort: [{ key: "score", descending: true }] });
  const byImported = query({ sort: [{ key: "importedAt", descending: true }] });
  assert.equal(queryIdentityKey(byScore), queryIdentityKey(byImported));
  assert.notEqual(querySortKey(byScore), querySortKey(byImported));
  // 换了筛选才算换结果集，才允许清空列表与回到顶部。
  const filtered = query({
    sort: [{ key: "score", descending: true }],
    filters: { tagIds: [3] },
  });
  assert.notEqual(queryIdentityKey(byScore), queryIdentityKey(filtered));
});

test("文件夹排序与单级键集排序的游标差异", () => {
  const byFolder = query({ sort: [{ key: "folder", descending: false }] });
  assert.equal(needsCursorRefresh(byFolder, true), true);
  // 单级键集游标在还有下一页时不需要为纯元数据写入重取。
  const byImported = query({ sort: [{ key: "importedAt", descending: true }] });
  assert.equal(needsCursorRefresh(byImported, true), false);
});

test("外部元数据写入没有 id 时重取可见行", () => {
  const external = change({
    source: "mcp",
    fields: ["score"],
    score: 4,
  });
  const plan = planDataChange(external, query(), context);
  assert.equal(plan.list, "refresh");
  assert.equal(plan.selection, "keep");
});

test("标签改名保留选择；删除正在筛选的标签清空选择", () => {
  const current = query({ filters: { tagIds: [7] } });
  const rename = change({ kind: "tagCatalog", tagId: 7, tagAction: "rename" });
  assert.deepEqual([...selectionAfterChange(new Set([1]), rename, current, [])], [1]);

  const remove = change({ kind: "tagCatalog", tagId: 7, tagAction: "delete" });
  assert.deepEqual([...selectionAfterChange(new Set([1]), remove, current, [])], []);
});

test("合并掉正在筛选的源标签也清空选择", () => {
  const current = query({ filters: { tagIds: [7] } });
  const merge = change({
    kind: "tagCatalog",
    tagId: 7,
    tagAction: "merge",
    targetTagId: 9,
  });
  assert.deepEqual([...selectionAfterChange(new Set([1]), merge, current, [])], []);
  assert.equal(planDataChange(merge, current, context).tagOptions, true);
});

test("任务与 MCP 变化映射到对应的变化类型", () => {
  assert.equal(dataChangeForTaskKind("thumbnail").kind, "thumbnails");
  assert.equal(dataChangeForTaskKind("aiPose").similarity, "clear");
  assert.equal(dataChangeForTaskKind("releaseGarbage").kind, "none");
  assert.equal(dataChangeForTaskKind("folderMove").clearSelection, true);
  assert.equal(dataChangeForTaskKind("optimize").kind, "content");
  // 临时区特征会让临时视图的特征筛选结果变化，并清空相似结果。
  assert.equal(dataChangeForTaskKind("tempFeatures").similarity, "clear");
  // 估算与分组任务不写图库数据。
  assert.equal(dataChangeForTaskKind("similarityGroups").kind, "none");
  assert.equal(dataChangeForTaskKind("optimizeEstimate").kind, "none");

  const mcp = dataChangeForMcp({
    tool: "images_update",
    change: { kind: "metadata", fields: ["tags"], tagAction: "remove" },
  });
  assert.equal(mcp.kind, "metadata");
  assert.equal(mcp.tagAction, "remove");
  assert.equal(mcp.source, "mcp");

  const trash = dataChangeForMcp({ tool: "images_trash", invalidateSimilarity: true });
  assert.equal(trash.kind, "membership");
  assert.equal(trash.clearSelection, true);
  assert.equal(trash.similarity, "clear");

  // 旧版后端没有结构化 change 时按工具名回退；认不出的工具按未知变化保守处理。
  assert.equal(dataChangeForMcp({ tool: "folders_modify" }).kind, "folder");
  assert.equal(dataChangeForMcp({ tool: "tags_list" }).kind, "unknown");
});

test("合并计划取最保守的动作", () => {
  const merged = combinePlans([
    planDataChange(change({ fields: ["score"], ids: [1], score: 3 }), query(), context),
    planDataChange(
      change({ kind: "membership", source: "task", similarity: "clear" }),
      query(),
      context,
    ),
  ]);
  assert.equal(merged.list, "refresh");
  assert.equal(merged.selection, "clear");
  assert.equal(merged.similarity, "clear");
});

test("标签候选只随标签变化与未知变化重算", () => {
  assert.equal(touchesTags(change({ fields: ["tags"] })), true);
  assert.equal(touchesTags(change({ kind: "tagCatalog" })), true);
  // 撤销/重做与认不出的外部写入可能改了标签，按保守口径重算。
  assert.equal(touchesTags(change({ kind: "unknown" })), true);
  assert.equal(touchesTags(change({ fields: ["score", "description"] })), false);
  assert.equal(touchesTags(change({ kind: "folder", fields: ["folder"] })), false);
  assert.equal(touchesTags(change({ kind: "features", feature: "pose" })), false);
});

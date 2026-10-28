import test from "node:test";
import assert from "node:assert/strict";
import {
  COLLECTION_SORT_LEVELS,
  DEFAULT_SORT_LEVELS,
  activeFilterCount,
  defaultBrowseState,
  effectiveSort,
  emptyFilters,
  imageActions,
  isGifImage,
  normalizeFilters,
  rowInTagScope,
  sortForViewChange,
  sortSummary,
  tagFilterState,
  type BrowseState,
} from "./model.ts";
import type { ImageRow } from "./types.ts";

function state(overrides: Partial<BrowseState> = {}): BrowseState {
  return { ...defaultBrowseState(), ...overrides };
}

function row(overrides: Partial<ImageRow> = {}): ImageRow {
  return {
    id: 1,
    contentHash: "hash",
    fileExt: "png",
    folderId: 2,
    folderPath: "人物/线稿",
    filePath: "G:/lib/人物/线稿/hash.png",
    fileSize: 1024,
    width: 100,
    height: 100,
    format: "png",
    hasAlpha: false,
    score: 3,
    importedAt: 1_700_000_000_000,
    modifiedAt: 1_700_000_000_000,
    deletedAt: null,
    thumbVersion: 1,
    thumbWidth: 100,
    thumbHeight: 100,
    tags: [],
    ...overrides,
  };
}

test("current-view filtering keeps the other conditions", () => {
  const current = state({
    search: "狐狸",
    filters: { ...defaultBrowseState().filters, folderIds: [2], tagIds: [7] },
  });
  const next = tagFilterState(current, {
    tagId: 9,
    action: "apply",
    scope: "view",
    collectionId: null,
  });
  assert.equal(next?.search, "狐狸");
  assert.deepEqual(next?.filters.folderIds, [2]);
  assert.deepEqual(next?.filters.tagIds, [9]);
});

test("scoped filtering drops search and unrelated filters", () => {
  const current = state({
    view: "collection",
    collectionId: 4,
    search: "狐狸",
    sort: COLLECTION_SORT_LEVELS,
    collectionSort: COLLECTION_SORT_LEVELS,
    filters: {
      ...defaultBrowseState().filters,
      folderIds: [2],
      extensions: ["png"],
      scores: [5],
      tagIds: [7],
    },
  });
  const next = tagFilterState(current, {
    tagId: 9,
    action: "apply",
    scope: "library",
    collectionId: 4,
  });
  assert.equal(next?.view, "all");
  assert.equal(next?.collectionId, null);
  assert.equal(next?.search, "");
  assert.deepEqual(next?.sort, DEFAULT_SORT_LEVELS);
  assert.deepEqual(next?.collectionSort, COLLECTION_SORT_LEVELS);
  assert.deepEqual(next?.filters, { ...defaultBrowseState().filters, tagIds: [9] });
});

test("folder scope keeps the folder subtree and nothing else", () => {
  const current = state({
    search: "狐狸",
    filters: { ...defaultBrowseState().filters, folderIds: [2], extensions: ["png"], tagIds: [7] },
  });
  const next = tagFilterState(current, {
    tagId: 9,
    action: "apply",
    scope: "folder",
    collectionId: null,
  });
  assert.deepEqual(next?.filters.folderIds, [2]);
  assert.deepEqual(next?.filters.extensions, []);
  assert.deepEqual(next?.filters.tagIds, [9]);
});

test("scopes without a target browsing state are unavailable", () => {
  const current = state();
  assert.equal(
    tagFilterState(current, { tagId: 9, action: "apply", scope: "folder", collectionId: null }),
    null,
  );
  assert.equal(
    tagFilterState(current, { tagId: 9, action: "apply", scope: "collection", collectionId: null }),
    null,
  );
});

test("append toggles the tag while remove drops it", () => {
  const current = state({ filters: { ...defaultBrowseState().filters, tagIds: [7] } });
  const appended = tagFilterState(current, {
    tagId: 9,
    action: "append",
    scope: "view",
    collectionId: null,
  });
  assert.deepEqual(appended?.filters.tagIds, [7, 9]);

  const toggledOff = tagFilterState(current, {
    tagId: 7,
    action: "append",
    scope: "view",
    collectionId: null,
  });
  assert.deepEqual(toggledOff?.filters.tagIds, []);

  const removed = tagFilterState(appended!, {
    tagId: 7,
    action: "remove",
    scope: "view",
    collectionId: null,
  });
  assert.deepEqual(removed?.filters.tagIds, [9]);
});

test("tag presence and exclusions count as filters", () => {
  assert.equal(activeFilterCount(emptyFilters()), 0);

  const missing = normalizeFilters({
    ...emptyFilters(),
    tagIds: [7],
    tagPresence: "missing",
  });
  // “一个标签都没有”与“含某些标签”互斥，前者赢。
  assert.deepEqual(missing.tagIds, []);
  assert.equal(activeFilterCount(missing), 1);

  const excluded = normalizeFilters({ ...emptyFilters(), excludeTagIds: [9, 3, 9] });
  assert.deepEqual(excluded.excludeTagIds, [3, 9]);
  assert.equal(activeFilterCount(excluded), 1);
});

test("global sort applies to every library view", () => {
  const current = state({
    sort: [
      { key: "score", descending: true },
      { key: "size", descending: false },
    ],
  });
  assert.deepEqual(effectiveSort({ ...current, view: "trash" }), current.sort);
  assert.deepEqual(effectiveSort({ ...current, view: "collection", collectionId: 1 }), current.sort);
});

test("collection order is stashed when leaving and restored when returning", () => {
  const inCollection = state({ view: "collection", collectionId: 1, sort: COLLECTION_SORT_LEVELS });
  const leaving = sortForViewChange(inCollection, "all");
  assert.deepEqual(leaving.sort, DEFAULT_SORT_LEVELS);
  assert.deepEqual(leaving.collectionSort, COLLECTION_SORT_LEVELS);

  const back = sortForViewChange(
    { ...inCollection, view: "all", sort: leaving.sort, collectionSort: leaving.collectionSort },
    "collection",
  );
  assert.deepEqual(back.sort, COLLECTION_SORT_LEVELS);
});

test("collection order is dropped from the effective sort outside a collection", () => {
  const current = state({ sort: COLLECTION_SORT_LEVELS });
  assert.deepEqual(effectiveSort({ ...current, view: "all" }), DEFAULT_SORT_LEVELS);
  assert.deepEqual(
    effectiveSort({
      ...current,
      view: "collection",
      collectionId: 1,
      search: "狐狸",
      searchScope: "library",
    }),
    DEFAULT_SORT_LEVELS,
  );
});

test("sort summary names the first levels", () => {
  assert.equal(sortSummary(DEFAULT_SORT_LEVELS), "导入时间 ↓");
  assert.equal(
    sortSummary([
      { key: "score", descending: true },
      { key: "importedAt", descending: false },
      { key: "size", descending: true },
    ]),
    "评分 ↓ + 导入时间 ↑ +1",
  );
});

test("trash actions follow the view, not the row state", () => {
  assert.deepEqual(imageActions(false), ["trash"]);
  assert.deepEqual(imageActions(true), ["restore", "delete"]);
});

test("rowInTagScope matches folder subtrees and trash state", () => {
  const item = row();
  const context = {
    folderPaths: ["人物"],
    folderDirect: false,
    hasCollection: true,
    similarityActive: false,
  };
  assert.equal(rowInTagScope(item, "folder", context), true);
  assert.equal(rowInTagScope(item, "folder", { ...context, folderPaths: ["背景"] }), false);
  assert.equal(rowInTagScope(item, "folder", { ...context, folderPaths: [""] }), true);
  assert.equal(
    rowInTagScope(item, "folder", { ...context, folderDirect: true, folderPaths: [""] }),
    false,
  );
  assert.equal(
    rowInTagScope(item, "folder", { ...context, folderDirect: true, folderPaths: ["人物"] }),
    false,
  );
  assert.equal(
    rowInTagScope(item, "folder", {
      ...context,
      folderDirect: true,
      folderPaths: ["人物/线稿"],
    }),
    true,
  );
  assert.equal(rowInTagScope(item, "collection", context), true);
  assert.equal(rowInTagScope(item, "library", context), true);
  assert.equal(
    rowInTagScope(row({ deletedAt: 1 }), "library", context),
    false,
  );
  assert.equal(rowInTagScope(item, "view", context), true);
  assert.equal(
    rowInTagScope(item, "view", { ...context, similarityActive: true }),
    false,
  );
});

test("isGifImage follows the imported format, not the extension", () => {
  assert.equal(isGifImage(row({ format: "gif" })), true);
  assert.equal(isGifImage(row({ format: "GIF" })), true);
  assert.equal(isGifImage(row({ format: "png", fileExt: "gif" })), false);
  assert.equal(isGifImage(row({ format: "webp", fileExt: "webp" })), false);
});

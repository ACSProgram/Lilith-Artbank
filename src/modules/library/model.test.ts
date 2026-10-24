import test from "node:test";
import assert from "node:assert/strict";
import {
  COLLECTION_SORT,
  DEFAULT_SORT,
  defaultBrowseState,
  rowInTagScope,
  sortForView,
  sortValueForRow,
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
    sort: COLLECTION_SORT,
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
  assert.deepEqual(next?.sort, DEFAULT_SORT);
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

test("sortForView only allows collection order inside a collection", () => {
  assert.deepEqual(sortForView("collection", DEFAULT_SORT), COLLECTION_SORT);
  assert.deepEqual(sortForView("all", COLLECTION_SORT), DEFAULT_SORT);
  assert.deepEqual(sortForView("all", DEFAULT_SORT), DEFAULT_SORT);
});

test("sortValueForRow follows the sort key", () => {
  const item = row({ score: 4, fileSize: 2048, importedAt: 99 });
  assert.equal(sortValueForRow(item, "importedAt"), 99);
  assert.equal(sortValueForRow(item, "score"), 4);
  assert.equal(sortValueForRow(item, "size"), 2048);
  assert.equal(sortValueForRow(item, "collectionPosition"), null);
});

test("rowInTagScope matches folder subtrees and trash state", () => {
  const item = row();
  const context = {
    folderPaths: ["人物"],
    hasCollection: true,
    similarityActive: false,
  };
  assert.equal(rowInTagScope(item, "folder", context), true);
  assert.equal(rowInTagScope(item, "folder", { ...context, folderPaths: ["背景"] }), false);
  assert.equal(rowInTagScope(item, "folder", { ...context, folderPaths: [""] }), true);
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

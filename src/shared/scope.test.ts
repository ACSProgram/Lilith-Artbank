import test from "node:test";
import assert from "node:assert/strict";
import {
  buildScope,
  defaultScopeChoice,
  scopeAvailability,
  scopeCountLabel,
  type ScopeChoice,
} from "./scope.ts";
import type { BatchTaskContextInfo } from "./batchTaskContext.ts";
import { emptyFilters } from "../modules/library/model.ts";
import type { LibraryQuery } from "../modules/library/types.ts";

const query: LibraryQuery = {
  view: "all",
  search: "",
  searchScope: "view",
  filters: { ...emptyFilters(), folderIds: [7] },
  sort: [],
};

function context(overrides: Partial<BatchTaskContextInfo> = {}): BatchTaskContextInfo {
  return {
    selectedIds: [],
    query,
    viewTotal: 120,
    folderId: 7,
    folderName: "人物/线稿",
    folderDirectTotal: 20,
    folderTotal: 120,
    collectionId: null,
    collectionName: null,
    collectionTotal: null,
    libraryTotal: 9_000,
    tempTotal: 3,
    ...overrides,
  };
}

test("build_scope_maps_each_choice_and_rejects_missing_context", () => {
  const base = context({ selectedIds: [1, 2], collectionId: 5, collectionName: "参考" });
  assert.deepEqual(buildScope("selected", base), { type: "selected", imageIds: [1, 2] });
  assert.deepEqual(buildScope("folder", base), {
    type: "folder",
    folderId: 7,
    includeSubfolders: true,
  });
  assert.deepEqual(buildScope("folder", base, { includeSubfolders: false }), {
    type: "folder",
    folderId: 7,
    includeSubfolders: false,
  });
  assert.deepEqual(buildScope("collection", base), { type: "collection", collectionId: 5 });
  assert.deepEqual(buildScope("view", base), { type: "view", query });
  assert.deepEqual(buildScope("temp", base), { type: "temp" });
  assert.deepEqual(buildScope("library", base), { type: "library" });

  const empty = context({ query: null, folderId: null, collectionId: null });
  assert.equal(buildScope("selected", empty), null);
  assert.equal(buildScope("folder", empty), null);
  assert.equal(buildScope("collection", empty), null);
  assert.equal(buildScope("view", empty), null);
});

test("default_scope_prefers_selection_then_view_then_library", () => {
  assert.equal(defaultScopeChoice(context({ selectedIds: [3] })), "selected");
  assert.equal(defaultScopeChoice(context()), "view");
  assert.equal(defaultScopeChoice(context({ query: null })), "library");
  assert.equal(defaultScopeChoice(context({ selectedIds: [3] }), "view"), "view");
  assert.equal(defaultScopeChoice(context({ query: null }), "view"), "library");
  assert.equal(defaultScopeChoice(null), "library");
});

test("folder_count_label_follows_subfolder_toggle", () => {
  const base = context({ selectedIds: [1] });
  assert.equal(scopeCountLabel("folder", base), "120 张（含子文件夹）");
  assert.equal(
    scopeCountLabel("folder", base, { includeSubfolders: false }),
    "20 张",
  );
  assert.equal(scopeCountLabel("selected", base), "1 张");
  assert.equal(scopeCountLabel("temp", base), "3 张");
  assert.equal(scopeCountLabel("library", base), "9,000 张");
  assert.equal(scopeCountLabel("collection", base), "");
});

test("availability_disables_choices_without_context", () => {
  const availability = scopeAvailability(context());
  assert.equal(availability.selected.enabled, false);
  assert.equal(availability.folder.enabled, true);
  assert.equal(availability.collection.enabled, false);
  assert.equal(availability.view.enabled, true);
  assert.equal(availability.temp.enabled, true);
  assert.equal(availability.library.enabled, true);

  const noTemp = scopeAvailability(context({ tempTotal: 0 }));
  assert.equal(noTemp.temp.enabled, false);

  const empty = scopeAvailability(null);
  for (const choice of Object.keys(empty) as ScopeChoice[]) {
    assert.equal(empty[choice].enabled, choice === "library");
  }
});

import test from "node:test";
import assert from "node:assert/strict";
import {
  applyFolderChange,
  buildFolderTree,
  childrenOf,
  defaultExpandedIds,
  expandedPaths,
  hasNestedSortable,
  needsFolderSort,
  resolveExpandedPaths,
  sortFolderOrders,
} from "./folderTreeModel.ts";
import type { FolderInfo } from "./types.ts";

function folder(
  id: number,
  path: string,
  sortIndex: number,
  directImageCount = 0,
  totalImageCount = directImageCount,
): FolderInfo {
  return { id, path, sortIndex, directImageCount, totalImageCount };
}

const folders: FolderInfo[] = [
  folder(1, "", 0),
  folder(2, "乙", 1, 5, 9),
  folder(3, "甲", 0, 1, 1),
  folder(4, "甲/子", 0, 4, 4),
  // 手动顺序排在最后，但按拼音应排在最前。
  folder(5, "丙", 5, 2, 2),
];

test("manual folder order follows sort index", () => {
  const tree = buildFolderTree(folders);
  assert.deepEqual(tree.nodes.map((node) => node.path), ["甲", "乙", "丙"]);
  assert.deepEqual(tree.byId.get(3)?.children.map((node) => node.path), ["甲/子"]);
});

test("one-time name sort orders direct children only", () => {
  const orders = sortFolderOrders(buildFolderTree(folders), 1, "name", false, false);
  assert.deepEqual(orders, [{ parentId: 1, folderIds: [5, 3, 2] }]);
});

test("one-time total sort respects direction and recursive levels", () => {
  const nested: FolderInfo[] = [
    ...folders,
    folder(6, "甲/子二", 1, 1, 3),
    folder(7, "乙/孙", 0, 1, 1),
    folder(8, "乙/孙二", 1, 1, 2),
  ];
  const recursive = sortFolderOrders(buildFolderTree(nested), 1, "total", true, true);
  assert.deepEqual(recursive, [
    { parentId: 1, folderIds: [2, 5, 3] },
    { parentId: 3, folderIds: [4, 6] },
    { parentId: 2, folderIds: [8, 7] },
  ]);

  const singleLevel = sortFolderOrders(buildFolderTree(nested), 1, "total", true, false);
  assert.deepEqual(singleLevel, [{ parentId: 1, folderIds: [2, 5, 3] }]);
});

test("sorting an inner folder only touches its subtree", () => {
  const nested: FolderInfo[] = [
    ...folders,
    folder(6, "甲/子二", 1, 1, 3),
  ];
  const orders = sortFolderOrders(buildFolderTree(nested), 3, "name", false, true);
  assert.deepEqual(orders, [{ parentId: 3, folderIds: [4, 6] }]);
});

test("sort affordances follow whether a subtree has siblings to order", () => {
  const tree = buildFolderTree(folders);
  assert.deepEqual(childrenOf(tree, 1).map((node) => node.path), ["甲", "乙", "丙"]);
  assert.deepEqual(childrenOf(tree, 3).map((node) => node.path), ["甲/子"]);
  assert.deepEqual(childrenOf(tree, 999), []);

  // 根有三个同级可排；"甲" 只有一个子文件夹，自身不可排。
  assert.equal(needsFolderSort(tree, 1), true);
  assert.equal(needsFolderSort(tree, 3), false);
  assert.equal(needsFolderSort(tree, 999), false);
  // 根的后代里没有别的同级，递归选项应保持禁用。
  assert.equal(hasNestedSortable(tree, 1), false);

  const nested = buildFolderTree([...folders, folder(6, "甲/子二", 1, 1, 3)]);
  assert.equal(needsFolderSort(nested, 3), true);
  assert.equal(hasNestedSortable(nested, 1), true);
  assert.equal(hasNestedSortable(nested, 3), false);
});

test("expansion state maps paths to ids and ignores missing paths", () => {
  const tree = buildFolderTree(folders);
  const ids = resolveExpandedPaths(["甲", "已删除"], tree.byPath);
  assert.deepEqual([...ids], [3]);
  assert.deepEqual(expandedPaths(new Set([3, 4]), tree.byId).sort(), ["甲", "甲/子"]);
  assert.deepEqual([...defaultExpandedIds(tree.nodes)], [3, 2, 5]);
});

test("folder change merges by id without reordering the rest", () => {
  // 改名：同 id 换成新路径，其余行保持原对象。
  const renamed = applyFolderChange(folders, {
    folders: [folder(3, "甲改", 0, 1, 1)],
    removedIds: [],
  });
  assert.equal(renamed.length, folders.length);
  assert.equal(renamed.find((item) => item.id === 3)?.path, "甲改");
  assert.equal(renamed.find((item) => item.id === 2), folders[1]);

  // 新建：不在本地树里的 id 追加到末尾。
  const created = applyFolderChange(folders, {
    folders: [folder(6, "丁", 6, 0, 0)],
    removedIds: [],
  });
  assert.equal(created.length, folders.length + 1);
  assert.equal(created.at(-1)?.path, "丁");

  // 删除：只剔除给定 id，并同时处理同一批里的更新。
  const removed = applyFolderChange(folders, {
    folders: [folder(2, "乙", 1, 0, 0)],
    removedIds: [3],
  });
  assert.deepEqual(
    removed.map((item) => item.id),
    [1, 2, 4, 5],
  );
  assert.equal(removed.find((item) => item.id === 2)?.directImageCount, 0);

  // 空变更不动任何行。
  assert.deepEqual(
    applyFolderChange(folders, { folders: [], removedIds: [] }),
    folders,
  );
});

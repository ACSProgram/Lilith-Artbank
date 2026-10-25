import test from "node:test";
import assert from "node:assert/strict";
import { DEFAULT_FOLDER_SORT, buildFolderTree } from "./folderTreeModel.ts";
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
  // 手动顺序排在最后，但按拼音应排在最前，用来区分“名称”与“手动”两种模式。
  folder(5, "丙", 5, 2, 2),
];

test("manual folder order follows sort index", () => {
  const tree = buildFolderTree(folders);
  assert.deepEqual(tree.nodes.map((node) => node.path), ["甲", "乙", "丙"]);
  assert.deepEqual(tree.byId.get(3)?.children.map((node) => node.path), ["甲/子"]);
});

test("manual order can be reversed", () => {
  const tree = buildFolderTree(folders, { ...DEFAULT_FOLDER_SORT, descending: true });
  assert.deepEqual(tree.nodes.map((node) => node.path), ["丙", "乙", "甲"]);
});

test("name and count sorts replace manual order", () => {
  const byName = buildFolderTree(folders, { mode: "name", descending: false });
  assert.deepEqual(byName.nodes.map((node) => node.path), ["丙", "甲", "乙"]);

  const byTotal = buildFolderTree(folders, { mode: "total", descending: true });
  assert.deepEqual(byTotal.nodes.map((node) => node.path), ["乙", "丙", "甲"]);

  const byDirect = buildFolderTree(folders, { mode: "direct", descending: false });
  assert.deepEqual(byDirect.nodes.map((node) => node.path), ["甲", "丙", "乙"]);
});

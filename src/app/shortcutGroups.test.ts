import { test } from "node:test";
import assert from "node:assert/strict";
import { duplicateShortcutKeys, SHORTCUT_GROUPS } from "./shortcutGroups.ts";

test("同一组里没有重复的键位组合", () => {
  for (const group of SHORTCUT_GROUPS) {
    assert.deepEqual(duplicateShortcutKeys(group), [], `「${group.title}」里有重复键位`);
  }
});

test("每条快捷键都有键位与说明，组名不重复", () => {
  const titles = new Set<string>();
  for (const group of SHORTCUT_GROUPS) {
    assert.ok(group.title.length > 0);
    assert.ok(!titles.has(group.title), `组名重复：${group.title}`);
    titles.add(group.title);
    for (const row of group.rows) {
      assert.ok(row.keys.length > 0, `「${group.title}」有一行没有键位`);
      assert.ok(row.keys.every((key) => key.trim().length > 0));
      assert.ok(row.action.trim().length > 0, `「${group.title}」有一行没有说明`);
    }
  }
});

test("重复检测能认出同一组内的同一个组合", () => {
  const group = {
    title: "测试",
    rows: [
      { keys: ["Ctrl", "Z"], action: "a" },
      { keys: ["Ctrl", "Z"], action: "b" },
    ],
  };
  assert.deepEqual(duplicateShortcutKeys(group), ["Ctrl+Z"]);
});

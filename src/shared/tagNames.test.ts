import test from "node:test";
import assert from "node:assert/strict";
import { mergeTagNames, normalizeTagName } from "./tagNames.ts";

test("normalize_tag_name_keeps_inner_delimiters", () => {
  assert.equal(normalizeTagName("  人物速写  "), "人物速写");
  assert.equal(normalizeTagName("a,b;c d"), "a,b;c d");
  assert.equal(normalizeTagName("   "), null);
  assert.equal(normalizeTagName(""), null);
});

test("merge_tag_names_dedupes_case_insensitively", () => {
  assert.deepEqual(mergeTagNames([], ["速写", "Sketch", "sketch"]), ["速写", "Sketch"]);
  assert.deepEqual(mergeTagNames(["速写"], ["速写", "参考"]), ["速写", "参考"]);
  assert.deepEqual(mergeTagNames(["Sketch"], ["sketch"]), ["Sketch"]);
});

test("merge_tag_names_applies_exclude_and_max", () => {
  assert.deepEqual(mergeTagNames([], ["已存在", "新增"], { exclude: ["已存在"] }), ["新增"]);
  assert.deepEqual(mergeTagNames(["一"], ["二", "三", "四"], { max: 3 }), ["一", "二", "三"]);
  assert.deepEqual(mergeTagNames(["一", "二"], ["三"], { max: 2 }), ["一", "二"]);
});

test("merge_tag_names_ignores_empty_additions", () => {
  assert.deepEqual(mergeTagNames([], ["", "  ", "标签"]), ["标签"]);
});

import test from "node:test";
import assert from "node:assert/strict";
import {
  filterTagSuggestions,
  mergeTagNames,
  normalizeTagName,
  tagSuggestionRequest,
} from "./tagNames.ts";

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

test("filter_tag_suggestions_handles_empty_input", () => {
  assert.deepEqual(filterTagSuggestions([], ["已选"]), []);
  assert.deepEqual(filterTagSuggestions([{ name: "  " }], []), []);
});

test("filter_tag_suggestions_keeps_order_and_count", () => {
  const items = [
    { name: "人物", imageCount: 12 },
    { name: "速写", imageCount: 3 },
  ];
  assert.deepEqual(filterTagSuggestions(items, []), items);
  assert.deepEqual(filterTagSuggestions(items, [], 1), [{ name: "人物", imageCount: 12 }]);
  assert.deepEqual(filterTagSuggestions(items, [], 0), []);
});

test("filter_tag_suggestions_excludes_blocked_case_insensitively", () => {
  const items = [{ name: "Sketch", imageCount: 5 }, { name: "参考" }];
  assert.deepEqual(filterTagSuggestions(items, [" sketch "]), [{ name: "参考" }]);
});

test("filter_tag_suggestions_dedupes_names", () => {
  const items = [
    { name: "Sketch", imageCount: 5 },
    { name: "sketch", imageCount: 9 },
    { name: "参考" },
  ];
  assert.deepEqual(filterTagSuggestions(items, []), [
    { name: "Sketch", imageCount: 5 },
    { name: "参考" },
  ]);
});

test("filter_tag_suggestions_does_not_pad_short_results", () => {
  assert.deepEqual(filterTagSuggestions([{ name: "唯一" }], ["其它"], 12), [{ name: "唯一" }]);
});

test("tag_suggestion_request_uses_debounce_only_for_text", () => {
  assert.deepEqual(tagSuggestionRequest(""), { needle: "", debounceMs: 0 });
  assert.deepEqual(tagSuggestionRequest("   "), { needle: "", debounceMs: 0 });
  assert.deepEqual(tagSuggestionRequest(" 人 "), { needle: "人", debounceMs: 160 });
});

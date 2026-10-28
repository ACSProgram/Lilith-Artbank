import test from "node:test";
import assert from "node:assert/strict";
import { scoreBars, topValueCounts } from "./folderStats.ts";

test("topValueCounts 保留前 N 项并合并其余", () => {
  const items = [
    { value: "jpg", count: 6 },
    { value: "png", count: 5 },
    { value: "webp", count: 4 },
    { value: "gif", count: 3 },
  ];
  assert.deepEqual(topValueCounts(items, 2), [
    { label: "jpg", count: 6 },
    { label: "png", count: 5 },
    { label: "其他（2 种）", count: 7 },
  ]);
});

test("topValueCounts 不足上限时不产生其他", () => {
  const items = [
    { value: "jpg", count: 2 },
    { value: "png", count: 1 },
  ];
  assert.deepEqual(topValueCounts(items, 6), [
    { label: "jpg", count: 2 },
    { label: "png", count: 1 },
  ]);
  assert.deepEqual(topValueCounts([], 6), []);
});

test("scoreBars 补齐 0-5 并保留缺省值", () => {
  assert.deepEqual(
    scoreBars([
      { value: "0", count: 2 },
      { value: "3", count: 4 },
      { value: "5", count: 1 },
    ]),
    [
      { label: "未评分", count: 2 },
      { label: "★", count: 0 },
      { label: "★★", count: 0 },
      { label: "★★★", count: 4 },
      { label: "★★★★", count: 0 },
      { label: "★★★★★", count: 1 },
    ],
  );
});

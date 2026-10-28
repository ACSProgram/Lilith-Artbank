import test from "node:test";
import assert from "node:assert/strict";
import {
  DONUT_OTHER_KEY,
  DONUT_UNSCORED_KEY,
  donutSegments,
  scoreSegments,
} from "./folderStats.ts";

test("donutSegments 保留前 N 项并把其余并入其他", () => {
  const items = [
    { value: "jpg", count: 6 },
    { value: "png", count: 5 },
    { value: "webp", count: 4 },
    { value: "gif", count: 3 },
    { value: "tif", count: 2 },
    { value: "bmp", count: 1 },
    { value: "psd", count: 1 },
  ];
  assert.deepEqual(donutSegments(items, 5), [
    { key: "jpg", label: "jpg", value: 6 },
    { key: "png", label: "png", value: 5 },
    { key: "webp", label: "webp", value: 4 },
    { key: "gif", label: "gif", value: 3 },
    { key: "tif", label: "tif", value: 2 },
    { key: DONUT_OTHER_KEY, label: "其他", value: 2 },
  ]);
});

test("donutSegments 不足上限时没有其他段，空输入返回空", () => {
  const items = [
    { value: "jpg", count: 2 },
    { value: "png", count: 1 },
  ];
  assert.deepEqual(donutSegments(items, 5), [
    { key: "jpg", label: "jpg", value: 2 },
    { key: "png", label: "png", value: 1 },
  ]);
  assert.deepEqual(donutSegments([], 5), []);
});

test("donutSegments 剔除 0 值的项", () => {
  const items = [
    { value: "jpg", count: 4 },
    { value: "png", count: 0 },
    { value: "webp", count: 3 },
  ];
  assert.deepEqual(donutSegments(items, 5), [
    { key: "jpg", label: "jpg", value: 4 },
    { key: "webp", label: "webp", value: 3 },
  ]);
});

test("scoreSegments 按 5 到 1 再到未评分排序并保留 0 张的档", () => {
  assert.deepEqual(
    scoreSegments([
      { value: "0", count: 2 },
      { value: "3", count: 4 },
      { value: "5", count: 1 },
    ]),
    [
      { key: "score-5", label: "★★★★★", value: 1 },
      { key: "score-4", label: "★★★★", value: 0 },
      { key: "score-3", label: "★★★", value: 4 },
      { key: "score-2", label: "★★", value: 0 },
      { key: "score-1", label: "★", value: 0 },
      { key: DONUT_UNSCORED_KEY, label: "未评分", value: 2 },
    ],
  );
  assert.equal(scoreSegments([]).length, 6);
});

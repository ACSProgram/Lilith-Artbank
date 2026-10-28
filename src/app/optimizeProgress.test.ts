import test from "node:test";
import assert from "node:assert/strict";
import {
  appendRuntimePoint,
  compareOptimizeEstimate,
  type RuntimeOptimizePoint,
} from "./optimizeProgress.ts";

test("采样间隔小于 2 秒丢弃", () => {
  let points: RuntimeOptimizePoint[] = [];
  points = appendRuntimePoint(points, {
    elapsedSeconds: 1,
    savingsBytes: 100,
    processed: 1,
  });
  points = appendRuntimePoint(points, {
    elapsedSeconds: 2.5,
    savingsBytes: 200,
    processed: 2,
  });
  assert.equal(points.length, 1);
  points = appendRuntimePoint(points, {
    elapsedSeconds: 3.2,
    savingsBytes: 200,
    processed: 2,
  });
  assert.equal(points.length, 2);
});

test("没有新进展时不取点，时间倒退直接忽略", () => {
  const start = [
    { elapsedSeconds: 3, savingsBytes: 100, processed: 1 },
  ];
  assert.deepEqual(
    appendRuntimePoint(start, {
      elapsedSeconds: 5,
      savingsBytes: 100,
      processed: 1,
    }),
    start,
  );
  assert.deepEqual(
    appendRuntimePoint(start, {
      elapsedSeconds: 2,
      savingsBytes: 999,
      processed: 9,
    }),
    start,
  );
});

test("超过上限时隔点丢弃且保留最后一点", () => {
  let points: RuntimeOptimizePoint[] = [];
  for (let index = 0; index < 241; index += 1) {
    points = appendRuntimePoint(
      points,
      {
        elapsedSeconds: index * 3,
        savingsBytes: index * 100,
        processed: index,
      },
      { maxPoints: 240 },
    );
  }
  assert.ok(points.length <= 240);
  assert.equal(points.at(-1)?.processed, 240);
  for (let index = 1; index < points.length; index += 1) {
    assert.ok(points[index].elapsedSeconds > points[index - 1].elapsedSeconds);
  }
});

test("预计与实际对照给出差异", () => {
  const comparison = compareOptimizeEstimate(
    { savingsBytes: 1_000, estimatedSeconds: 60, processCount: 50 },
    {
      savedBytes: 900,
      elapsedSeconds: 70,
      completed: 48,
      status: "completed",
    },
  );
  assert.equal(comparison.completed, true);
  assert.deepEqual(
    comparison.rows.map((row) => [row.key, row.estimated, row.actual, row.delta]),
    [
      ["savings", 1_000, 900, -100],
      ["duration", 60, 70, 10],
      ["count", 50, 48, -2],
    ],
  );
});

test("取消或缺失字段时不崩", () => {
  const comparison = compareOptimizeEstimate(
    { savingsBytes: 1_000, estimatedSeconds: 60, processCount: 50 },
    { status: "cancelled" },
  );
  assert.equal(comparison.completed, false);
  assert.equal(comparison.rows[0].actual, 0);
  assert.equal(comparison.rows[1].delta, -60);
  const empty = compareOptimizeEstimate(
    { savingsBytes: 0, estimatedSeconds: 0, processCount: 0 },
    {},
  );
  assert.equal(empty.rows.length, 3);
  assert.equal(empty.completed, false);
});

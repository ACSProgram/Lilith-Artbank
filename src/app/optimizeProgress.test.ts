import test from "node:test";
import assert from "node:assert/strict";
import {
  actualAtCoverage,
  actualCurvePoints,
  appendRuntimePoint,
  compareOptimizeEstimate,
  curveAt,
  earlySavingsHint,
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

test("预计与实际对照按三项给出实际与预计", () => {
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
    comparison.rows.map((row) => [row.key, row.estimated, row.actual]),
    [
      ["savings", 1_000, 900],
      ["duration", 60, 70],
      ["count", 50, 48],
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
  assert.equal(comparison.rows[1].actual, 0);
  const empty = compareOptimizeEstimate(
    { savingsBytes: 0, estimatedSeconds: 0, processCount: 0 },
    {},
  );
  assert.equal(empty.rows.length, 3);
  assert.equal(empty.completed, false);
});

test("actualCurvePoints 按候选张数折算覆盖率且不回退", () => {
  const points: RuntimeOptimizePoint[] = [
    { elapsedSeconds: 2, savingsBytes: 0, processed: 0 },
    { elapsedSeconds: 4, savingsBytes: 100, processed: 10 },
    { elapsedSeconds: 6, savingsBytes: 260, processed: 20 },
    // 覆盖率回退的采样丢掉。
    { elapsedSeconds: 8, savingsBytes: 900, processed: 3 },
    { elapsedSeconds: 10, savingsBytes: 400, processed: 40 },
  ];
  assert.deepEqual(actualCurvePoints(points, 40), [
    { coverage: 0, savingsBytes: 0 },
    { coverage: 0.25, savingsBytes: 100 },
    { coverage: 0.5, savingsBytes: 260 },
    { coverage: 1, savingsBytes: 400 },
  ]);
  // 没有候选张数时算不出覆盖率，不画实际线。
  assert.deepEqual(actualCurvePoints(points, 0), []);
  assert.deepEqual(actualCurvePoints([], 40), []);
});

test("全跳过的区间仍然产生点，末点覆盖率等于 settled / total", () => {
  // 一批全被跳过：节省恒为 0，但横轴必须继续走到 100%。
  const points: RuntimeOptimizePoint[] = [
    { elapsedSeconds: 2, savingsBytes: 0, processed: 5 },
    { elapsedSeconds: 4, savingsBytes: 0, processed: 12 },
    { elapsedSeconds: 6, savingsBytes: 0, processed: 20 },
  ];
  const curve = actualCurvePoints(points, 20);
  assert.equal(curve.length, 3);
  assert.deepEqual(
    curve.map((point) => point.coverage),
    [0.25, 0.6, 1],
  );
  assert.ok(curve.every((point) => point.savingsBytes === 0));
  // 覆盖率单调不减。
  for (let index = 1; index < curve.length; index += 1) {
    assert.ok(curve[index].coverage > curve[index - 1].coverage);
  }
  assert.equal(curve.at(-1)?.coverage, 1);
});

test("actualAtCoverage 只在走到该覆盖率后给值", () => {
  const points = actualCurvePoints(
    [
      { elapsedSeconds: 2, savingsBytes: 120, processed: 10 },
      { elapsedSeconds: 4, savingsBytes: 300, processed: 30 },
    ],
    50,
  );
  // 里程碑取值用「已经走过的最后一个采样」，不外插。
  assert.equal(actualAtCoverage(points, 0.2), 120);
  assert.equal(actualAtCoverage(points, 0.5), 120);
  assert.equal(actualAtCoverage(points, 0.6), 300);
  assert.equal(actualAtCoverage(points, 0.9), null);
  assert.equal(actualAtCoverage([], 0.2), null);
});

test("curveAt 在里程碑之间线性插值", () => {
  const curve = [
    { coverage: 0, savingsBytes: 0, elapsedSeconds: 0 },
    { coverage: 0.4, savingsBytes: 400, elapsedSeconds: 40 },
    { coverage: 1, savingsBytes: 1_000, elapsedSeconds: 100 },
  ];
  assert.equal(curveAt(curve, 0.2).savingsBytes, 200);
  assert.equal(curveAt(curve, 0.4).savingsBytes, 400);
  // 超出末点就停在末点，不外插。
  assert.equal(curveAt(curve, 1.5).savingsBytes, 1_000);
  assert.deepEqual(curveAt([], 0.3), {
    coverage: 0.3,
    savingsBytes: 0,
    elapsedSeconds: 0,
  });
});

test("前期收益偏小时给一句话预告", () => {
  const earlyHeavy = [
    { coverage: 0, savingsBytes: 0, elapsedSeconds: 0 },
    { coverage: 0.2, savingsBytes: 100, elapsedSeconds: 20 },
    { coverage: 1, savingsBytes: 1_000, elapsedSeconds: 100 },
  ];
  // 前 20% 只有 10% 收益（份额的一半）算正常，不提示。
  assert.equal(earlySavingsHint(earlyHeavy, 1_000), null);

  const backHeavy = [
    { coverage: 0, savingsBytes: 0, elapsedSeconds: 0 },
    { coverage: 0.2, savingsBytes: 40, elapsedSeconds: 20 },
    { coverage: 1, savingsBytes: 1_000, elapsedSeconds: 100 },
  ];
  const hint = earlySavingsHint(backHeavy, 1_000);
  assert.ok(hint && hint.includes("4%"), `{hint}`);

  const noneEarly = [
    { coverage: 0, savingsBytes: 0, elapsedSeconds: 0 },
    { coverage: 0.2, savingsBytes: 0, elapsedSeconds: 20 },
    { coverage: 1, savingsBytes: 1_000, elapsedSeconds: 100 },
  ];
  assert.ok(earlySavingsHint(noneEarly, 1_000)?.includes("没有可压缩的空间"));

  // 只有一个点、或者总收益为 0 时不提示。
  assert.equal(earlySavingsHint([{ coverage: 0, savingsBytes: 0, elapsedSeconds: 0 }], 1_000), null);
  assert.equal(earlySavingsHint(backHeavy, 0), null);
});

import test from "node:test";
import assert from "node:assert/strict";
import {
  formatRelative,
  reportIsClean,
  segmentGarbageRatio,
  type InspectionReport,
  type SegmentStat,
} from "./maintenanceTypes.ts";

function segment(overrides: Partial<SegmentStat> = {}): SegmentStat {
  return {
    id: 1,
    fileName: "001.pack",
    size: 1_000,
    used: 600,
    blobs: 12,
    garbage: 400,
    ...overrides,
  };
}

function report(overrides: Partial<InspectionReport> = {}): InspectionReport {
  return {
    taskId: "t1",
    libraryId: "lib",
    mode: "fast",
    scannedFolders: 3,
    scannedRows: 100,
    scannedFiles: 100,
    missingTotal: 0,
    orphanTotal: 0,
    sizeMismatchTotal: 0,
    hashMismatchTotal: 0,
    nonContentFiles: 0,
    changedDuringScan: 0,
    hashChecked: 0,
    missing: [],
    orphans: [],
    sizeMismatch: [],
    hashMismatch: [],
    truncated: false,
    elapsedMs: 1200,
    finishedAt: 0,
    ...overrides,
  };
}

test("段垃圾率按段文件大小算，空段不除零", () => {
  assert.equal(segmentGarbageRatio(segment()), 0.4);
  assert.equal(segmentGarbageRatio(segment({ size: 0, garbage: 0 })), 0);
  assert.equal(segmentGarbageRatio(segment({ size: 100, used: 100, garbage: 0 })), 0);
});

test("四种问题都为 0 才算干净", () => {
  assert.equal(reportIsClean(report()), true);
  assert.equal(reportIsClean(report({ orphanTotal: 1, orphans: [] })), false);
  assert.equal(reportIsClean(report({ hashMismatchTotal: 2 })), false);
  assert.equal(reportIsClean(report({ missingTotal: 1, sizeMismatchTotal: 1 })), false);
});

test("上次回收写成相对时间，一天以上改写成日期", () => {
  const now = Date.UTC(2026, 9, 28, 12, 0, 0);
  assert.equal(formatRelative(now - 20_000, now), "刚刚");
  assert.equal(formatRelative(now - 5 * 60_000, now), "5 分钟前");
  assert.equal(formatRelative(now - 3 * 60 * 60_000, now), "3 小时前");
  // 超过一天不再数小时，写成日期，避免出现「27 小时前」。
  assert.equal(formatRelative(now - 3 * 24 * 60 * 60_000, now), new Date(now - 3 * 24 * 60 * 60_000).toLocaleDateString());
  // 时钟回拨时不出现负数。
  assert.equal(formatRelative(now + 5_000, now), "刚刚");
});

import test from "node:test";
import assert from "node:assert/strict";
import type { EditableSource, GeometryState } from "./contracts/edit.ts";
import {
  clampCrop,
  fitZoom,
  fitZoomForSize,
  flipGeometry,
  gifStaticPreview,
  hasPixelChanges,
  initialGeometry,
  rectBetween,
  rescaleZoom,
  resizeForNewBase,
  rotateGeometry,
} from "./editGeometry.ts";

function source(overrides: Partial<EditableSource> = {}): EditableSource {
  return {
    format: "png",
    hasAlpha: false,
    width: 600,
    height: 400,
    displayWidth: 600,
    displayHeight: 400,
    ...overrides,
  };
}

function withCrop(crop: GeometryState["crop"], overrides: Partial<GeometryState> = {}) {
  return { ...initialGeometry(source()), crop, ...overrides };
}

test("rotating four times returns the original crop and flags", () => {
  const image = source();
  const start = withCrop({ x: 10, y: 20, width: 100, height: 50 });
  let next = start;
  for (let index = 0; index < 4; index += 1) {
    next = rotateGeometry(next, image, 1);
  }
  assert.deepEqual(next, start);
});

test("rotating transforms the crop without changing its area", () => {
  const next = rotateGeometry(
    withCrop({ x: 10, y: 20, width: 100, height: 50 }),
    source(),
    1,
  );
  assert.equal(next.rotate, 1);
  assert.deepEqual(next.crop, { x: 330, y: 10, width: 50, height: 100 });
  assert.equal(next.crop!.width * next.crop!.height, 100 * 50);
  assert.deepEqual(next.resize, null);
});

test("flipping twice returns to the original crop and flags", () => {
  const image = source();
  const start = withCrop({ x: 10, y: 20, width: 100, height: 50 });
  const once = flipGeometry(start, image, "horizontal");
  assert.deepEqual(once.crop, { x: 490, y: 20, width: 100, height: 50 });
  const twice = flipGeometry(once, image, "horizontal");
  assert.deepEqual(twice, start);
});

test("display horizontal flip after a 90° rotation toggles the source vertical flip", () => {
  const image = source();
  const rotated = rotateGeometry(withCrop(null), image, 1);
  const flipped = flipGeometry(rotated, image, "horizontal");
  assert.equal(flipped.rotate, 1);
  assert.equal(flipped.flipHorizontal, false);
  assert.equal(flipped.flipVertical, true);
});

test("resize follows the new base after rotation", () => {
  const image = source();
  const state = withCrop(null, { resize: { width: 300, height: 200 } });
  const rotated = rotateGeometry(state, image, 1);
  assert.deepEqual(rotated.resize, { width: 200, height: 300 });
});

test("resize equal to the old base is dropped", () => {
  const image = source();
  const state = withCrop(null, { resize: { width: 600, height: 400 } });
  const rotated = rotateGeometry(state, image, 1);
  assert.equal(rotated.resize, null);
});

test("resizeForNewBase keeps the relative scale", () => {
  assert.deepEqual(
    resizeForNewBase({ width: 400, height: 100 }, { width: 800, height: 200 }, { width: 200, height: 800 }),
    { width: 100, height: 400 },
  );
});

test("quality change counts as a pixel change only for lossy formats", () => {
  const image = source();
  const quality = withCrop(null, { quality: 60 });
  assert.equal(hasPixelChanges(image, quality), false);

  const jpeg = source({ format: "jpeg" });
  const jpegBase = initialGeometry(jpeg);
  assert.equal(hasPixelChanges(jpeg, { ...jpegBase, quality: 60 }), true);
  assert.equal(hasPixelChanges(jpeg, jpegBase), false);
  assert.equal(
    hasPixelChanges(jpeg, { ...jpegBase, outputFormat: "png" }),
    true,
  );
});

test("fitZoomForSize keeps the displayed size when the preview is proportional", () => {
  const placeholder = fitZoomForSize(4000, 3000, 0, 1000, 800);
  const preview = fitZoomForSize(4096, 3072, 0, 1000, 800);
  assert.ok(Math.abs(4000 * placeholder - 4096 * preview) < 1e-6);
  assert.ok(Math.abs(3000 * placeholder - 3072 * preview) < 1e-6);
});

test("fitZoomForSize swaps dimensions for odd rotations", () => {
  assert.equal(
    fitZoomForSize(3000, 4000, 1, 1000, 800),
    fitZoomForSize(4000, 3000, 0, 1000, 800),
  );
});

test("fitZoomForSize falls back to 1 and clamps the result", () => {
  assert.equal(fitZoomForSize(0, 100, 0, 1000, 800), 1);
  assert.equal(fitZoomForSize(100, 100, 0, 0, 800), 1);
  assert.equal(fitZoomForSize(100_000, 100, 0, 1000, 800), 0.02);
  assert.equal(fitZoomForSize(10, 10, 0, 1000, 800), 1);
});

test("fitZoom reads the source display size", () => {
  const image = source({ displayWidth: 3000, displayHeight: 4000 });
  assert.equal(
    fitZoom(image, 0, 1000, 800),
    fitZoomForSize(3000, 4000, 0, 1000, 800),
  );
});

test("rescaleZoom keeps the on-screen width when the display size changes", () => {
  const zoom = rescaleZoom(0.1, 8000, 4096);
  assert.ok(Math.abs(zoom - 0.1953125) < 1e-6);
  assert.ok(Math.abs(8000 * 0.1 - 4096 * zoom) < 1e-6);
  assert.equal(rescaleZoom(0.5, 0, 4096), 0.5);
  assert.equal(rescaleZoom(0.5, 4096, 0), 0.5);
  assert.equal(rescaleZoom(0.0001, 1, 8000), 0.02);
  assert.equal(rescaleZoom(8, 8000, 1), 8);
});

test("gifStaticPreview only matches GIFs with a generated preview", () => {
  assert.equal(gifStaticPreview({ format: "gif", preview: true }), true);
  assert.equal(gifStaticPreview({ format: "gif", preview: false }), false);
  assert.equal(gifStaticPreview({ format: "png", preview: true }), false);
  assert.equal(gifStaticPreview(null), false);
});

test("clampCrop rounds fractional pointer coordinates to whole pixels", () => {
  const rect = rectBetween({ x: 4.2, y: 8.6 }, { x: 188.4, y: 144.4 }, 262, 212);
  assert.ok(rect);
  const crop = clampCrop(rect, 262, 212);
  assert.deepEqual(crop, { x: 4, y: 9, width: 184, height: 136 });
  for (const value of Object.values(crop)) {
    assert.ok(Number.isInteger(value), `${value} 应该是整数`);
  }
});

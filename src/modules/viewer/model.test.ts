import test from "node:test";
import assert from "node:assert/strict";
import {
  flipGeometry,
  hasPixelChanges,
  initialGeometry,
  resizeForNewBase,
  rotateGeometry,
  type GeometryState,
} from "./model.ts";
import type { ViewerSource } from "./types.ts";

function source(overrides: Partial<ViewerSource> = {}): ViewerSource {
  return {
    imageId: 1,
    contentHash: "hash",
    fileExt: "png",
    format: "png",
    fileSize: 1000,
    thumbVersion: 1,
    hasAlpha: false,
    width: 600,
    height: 400,
    displayWidth: 600,
    displayHeight: 400,
    preview: false,
    downsampled: false,
    editable: true,
    deleted: false,
    maxEditPixels: 50_000_000,
    url: "",
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

  const jpeg = source({ format: "jpeg", fileExt: "jpg" });
  const jpegBase = initialGeometry(jpeg);
  assert.equal(hasPixelChanges(jpeg, { ...jpegBase, quality: 60 }), true);
  assert.equal(hasPixelChanges(jpeg, jpegBase), false);
  assert.equal(
    hasPixelChanges(jpeg, { ...jpegBase, outputFormat: "png" }),
    true,
  );
});

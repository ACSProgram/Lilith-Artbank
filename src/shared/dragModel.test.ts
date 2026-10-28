import test from "node:test";
import assert from "node:assert/strict";
import { directRowAccepts, dropPositionFor } from "./dragModel.ts";

test("images always drop inside a folder row", () => {
  for (const ratio of [0, 0.24, 0.5, 0.76, 1]) {
    assert.equal(dropPositionFor("images", ratio), "inside");
  }
});

test("folder rows keep the before / inside / after thirds", () => {
  assert.equal(dropPositionFor("folders", 0), "before");
  assert.equal(dropPositionFor("folders", 0.249), "before");
  assert.equal(dropPositionFor("folders", 0.25), "inside");
  assert.equal(dropPositionFor("folders", 0.5), "inside");
  assert.equal(dropPositionFor("folders", 0.749), "inside");
  assert.equal(dropPositionFor("folders", 0.75), "inside");
  assert.equal(dropPositionFor("folders", 0.751), "after");
  assert.equal(dropPositionFor("folders", 1), "after");
});

test("the direct row only accepts images", () => {
  assert.equal(directRowAccepts("images"), true);
  assert.equal(directRowAccepts("folders"), false);
});

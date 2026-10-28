import test from "node:test";
import assert from "node:assert/strict";
import { buildQuery, defaultBrowseState } from "./model.ts";
import { createStatsStore, statsKey } from "./statsStore.ts";
import type { LibraryQuery, StatsRequest, StatsSnapshot } from "./types.ts";

function query(search = ""): LibraryQuery {
  return buildQuery({ ...defaultBrowseState(), search });
}

function wait(ms = 25): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function summary(total: number): StatsSnapshot {
  return { profile: "summary", data: { total } };
}

function facets(total: number): StatsSnapshot {
  return {
    profile: "facets",
    data: { total, folders: [], extensions: [], scores: [], tags: [] },
  };
}

test("statsKey 区分 profile，忽略排序", () => {
  const base = query("人物");
  const sorted = { ...base, sort: [{ key: "score" as const, descending: false }] };
  assert.equal(statsKey("summary", base), statsKey("summary", sorted));
  assert.notEqual(statsKey("summary", base), statsKey("facets", base));
  assert.notEqual(statsKey("summary", base), statsKey("summary", query("风景")));
});

test("同一键并发 ensure 只发一次请求", async () => {
  let calls = 0;
  const gate = deferred<StatsSnapshot>();
  const store = createStatsStore({
    debounceMs: 5,
    fetch: () => {
      calls += 1;
      return gate.promise;
    },
  });
  store.ensure("summary", query());
  store.ensure("summary", query());
  await wait(1);
  assert.equal(calls, 1);
  gate.resolve(summary(3));
  await wait(1);
  assert.equal(store.get("summary", query())?.data?.total, 3);
});

test("facets 成功后派生 summary，不重复 count", async () => {
  let calls = 0;
  const requests: StatsRequest[] = [];
  const store = createStatsStore({
    debounceMs: 5,
    fetch: async (request) => {
      calls += 1;
      requests.push(request);
      return facets(7);
    },
  });
  store.ensure("facets", query());
  await wait(5);
  assert.equal(calls, 1);
  assert.equal(store.get("summary", query())?.data?.total, 7);
  assert.equal((await store.fetch("summary", query())).total, 7);
  assert.equal(calls, 1, "summary 从 facets 派生，不应再请求");
  assert.equal(requests[0].profile, "facets");
});

test("invalidate 后保留旧值并后台刷新", async () => {
  let calls = 0;
  let total = 1;
  const store = createStatsStore({
    debounceMs: 5,
    fetch: async () => {
      calls += 1;
      return summary(total);
    },
  });
  store.ensure("summary", query());
  await wait(5);
  assert.equal(store.get("summary", query())?.data?.total, 1);

  total = 2;
  store.invalidate();
  assert.equal(store.get("summary", query())?.data?.total, 1, "旧值仍然可见");
  store.ensure("summary", query());
  await wait(15);
  assert.equal(calls, 2);
  assert.equal(store.get("summary", query())?.data?.total, 2);
});

test("失败保留旧值，retry 可恢复", async () => {
  let fail = false;
  let total = 4;
  const store = createStatsStore({
    debounceMs: 5,
    fetch: async () => {
      if (fail) throw new Error("统计失败");
      return summary(total);
    },
  });
  store.ensure("summary", query());
  await wait(5);
  fail = true;
  total = 9;
  store.invalidate();
  store.ensure("summary", query());
  await wait(15);
  const failed = store.get("summary", query());
  assert.equal(failed?.data?.total, 4);
  assert.equal(failed?.error, "统计失败");

  fail = false;
  store.retry("summary", query());
  await wait(5);
  assert.equal(store.get("summary", query())?.data?.total, 9);
  assert.equal(store.get("summary", query())?.error, null);
});

test("容量上限按最久未用淘汰", async () => {
  const store = createStatsStore({
    capacity: 2,
    debounceMs: 5,
    fetch: async () => summary(1),
  });
  const first = query("a");
  const second = query("b");
  const third = query("c");
  for (const item of [first, second, third]) {
    store.ensure("summary", item);
    await wait(5);
  }
  assert.equal(store.get("summary", first), null);
  assert.ok(store.get("summary", second));
  assert.ok(store.get("summary", third));
});

test("clear 丢弃旧响应并清空缓存", async () => {
  const gate = deferred<StatsSnapshot>();
  const store = createStatsStore({
    debounceMs: 5,
    fetch: () => gate.promise,
  });
  store.ensure("summary", query());
  await wait(1);
  store.clear();
  gate.resolve(summary(5));
  await wait(5);
  assert.equal(store.get("summary", query()), null);
});

test("dispose 后不再接受请求", async () => {
  let calls = 0;
  const store = createStatsStore({
    debounceMs: 5,
    fetch: async () => {
      calls += 1;
      return summary(1);
    },
  });
  store.dispose();
  store.ensure("summary", query());
  await wait(5);
  assert.equal(calls, 0);
});

import { errorMessage } from "../../shared/errors.ts";
import { queryIdentityKey } from "./model.ts";
import type {
  FilterCounts,
  FolderStats,
  LibraryQuery,
  StatsProfile,
  StatsRequest,
  StatsSnapshot,
  SummaryStats,
} from "./types";

export interface StatsDataMap {
  facets: FilterCounts;
  summary: SummaryStats;
  folderDetail: FolderStats;
}

export interface StatsEntry<T> {
  data: T | null;
  status: "loading" | "ready" | "refreshing" | "error";
  /** 写入或刷新开始时的 store 版本；小于当前版本即为旧值。 */
  version: number;
  error: string | null;
  /** summary 由 facets 派生时记录来源，避免重复 count。 */
  derivedFromFacets?: boolean;
}

export interface StatsStore {
  subscribe(listener: () => void): () => void;
  getVersion(): number;
  get<P extends StatsProfile>(
    profile: P,
    query: LibraryQuery,
  ): StatsEntry<StatsDataMap[P]> | null;
  ensure<P extends StatsProfile>(profile: P, query: LibraryQuery): void;
  fetch<P extends StatsProfile>(
    profile: P,
    query: LibraryQuery,
  ): Promise<StatsDataMap[P]>;
  invalidate(): void;
  retry<P extends StatsProfile>(profile: P, query: LibraryQuery): void;
  clear(): void;
  dispose(): void;
}

export const STATS_CACHE_LIMIT = 48;
const DEFAULT_DEBOUNCE_MS = 160;

export function statsKey(profile: StatsProfile, query: LibraryQuery): string {
  return `${profile}\u0000${queryIdentityKey(query)}`;
}

type InternalEntry = StatsEntry<unknown> & { usedAt: number };

interface StatsStoreOptions {
  fetch: (request: StatsRequest) => Promise<StatsSnapshot>;
  capacity?: number;
  debounceMs?: number;
}

export function createStatsStore(options: StatsStoreOptions): StatsStore {
  const capacity = options.capacity ?? STATS_CACHE_LIMIT;
  const debounceMs = options.debounceMs ?? DEFAULT_DEBOUNCE_MS;
  const entries = new Map<string, InternalEntry>();
  const timers = new Map<string, ReturnType<typeof setTimeout>>();
  const inflight = new Map<string, Promise<void>>();
  const inflightTokens = new Map<string, number>();
  const generations = new Map<string, number>();
  const staleAfterInflight = new Set<string>();
  const listeners = new Set<() => void>();

  let version = 0;
  let epoch = 0;
  let usedClock = 0;
  let requestCounter = 0;
  let disposed = false;

  const notify = () => {
    for (const listener of [...listeners]) listener();
  };

  const touch = (key: string) => {
    const entry = entries.get(key);
    if (entry) {
      usedClock += 1;
      entry.usedAt = usedClock;
    }
  };

  const enforceCapacity = () => {
    while (entries.size > capacity) {
      let oldestKey: string | null = null;
      let oldestUsed = Number.POSITIVE_INFINITY;
      for (const [key, entry] of entries) {
        if (inflight.has(key)) continue;
        if (entry.usedAt < oldestUsed) {
          oldestUsed = entry.usedAt;
          oldestKey = key;
        }
      }
      if (oldestKey == null) return;
      entries.delete(oldestKey);
      // 被淘汰的键不再有缓存条目；清掉代次与旧响应标记，避免键表无限增长。
      // 待发请求保留：它仍可能被当前界面需要，回来后按普通写入重新入缓存。
      generations.delete(oldestKey);
      staleAfterInflight.delete(oldestKey);
    }
  };

  const update = (key: string, entry: InternalEntry) => {
    entries.set(key, entry);
    touch(key);
    enforceCapacity();
    notify();
  };

  const schedule = <P extends StatsProfile>(
    profile: P,
    query: LibraryQuery,
    key: string,
  ) => {
    const existing = timers.get(key);
    if (existing != null) clearTimeout(existing);
    const timer = setTimeout(() => {
      timers.delete(key);
      if (inflight.has(key)) return;
      const current = entries.get(key);
      if (current?.data != null && current.version === version) return;
      void request(profile, query, key);
    }, debounceMs);
    timers.set(key, timer);
  };

  const deriveSummary = (query: LibraryQuery): SummaryStats | null => {
    const facets = entries.get(statsKey("facets", query));
    if (facets?.data == null) return null;
    const data = facets.data as FilterCounts;
    const summaryKey = statsKey("summary", query);
    const entry = entries.get(summaryKey);
    if (entry?.data != null && entry.derivedFromFacets && entry.version >= facets.version) {
      return entry.data as SummaryStats;
    }
    update(summaryKey, {
      data: { total: data.total },
      status: facets.status === "refreshing" ? "refreshing" : "ready",
      version: facets.version,
      error: facets.error,
      derivedFromFacets: true,
      usedAt: 0,
    });
    return { total: data.total };
  };

  const request = <P extends StatsProfile>(
    profile: P,
    query: LibraryQuery,
    key: string,
  ): Promise<void> => {
    if (disposed) return Promise.resolve();
    const running = inflight.get(key);
    if (running) return running;

    const requestEpoch = epoch;
    const requestVersion = version;
    const requestToken = (requestCounter += 1);
    const generation = (generations.get(key) ?? 0) + 1;
    generations.set(key, generation);

    const previous = entries.get(key);
    entries.set(key, {
      data: previous?.data ?? null,
      status: previous?.data != null ? "refreshing" : "loading",
      version: previous?.version ?? requestVersion,
      error: null,
      derivedFromFacets: previous?.derivedFromFacets,
      usedAt: previous?.usedAt ?? 0,
    });
    touch(key);
    notify();

    let settle!: () => void;
    const promise = new Promise<void>((resolve) => {
      settle = resolve;
    });
    inflight.set(key, promise);
    inflightTokens.set(key, requestToken);

    void (async () => {
      try {
        const snapshot = await options.fetch({ profile, query });
        if (disposed || requestEpoch !== epoch) return;
        if (generation !== generations.get(key)) return;
        if (snapshot.profile !== profile) {
          throw new Error("统计返回的 profile 不一致");
        }
        const status = requestVersion === version ? "ready" : "refreshing";
        update(key, {
          data: snapshot.data,
          status,
          version: requestVersion,
          error: null,
          usedAt: 0,
        });
        if (profile === "facets") {
          const facets = snapshot.data as FilterCounts;
          const summaryKey = statsKey("summary", query);
          update(summaryKey, {
            data: { total: facets.total },
            status,
            version: requestVersion,
            error: null,
            derivedFromFacets: true,
            usedAt: 0,
          });
        }
      } catch (cause) {
        if (disposed || requestEpoch !== epoch) return;
        if (generation !== generations.get(key)) return;
        const current = entries.get(key);
        const message = errorMessage(cause);
        entries.set(key, {
          data: current?.data ?? null,
          status: current?.data != null ? "ready" : "error",
          version: current?.version ?? requestVersion,
          error: message,
          derivedFromFacets: current?.derivedFromFacets,
          usedAt: current?.usedAt ?? 0,
        });
        notify();
      } finally {
        if (inflightTokens.get(key) === requestToken) {
          inflight.delete(key);
          inflightTokens.delete(key);
        }
        if (staleAfterInflight.delete(key) && !disposed) {
          schedule(profile, query, key);
        }
        settle();
      }
    })();

    return promise;
  };

  const ensure = <P extends StatsProfile>(profile: P, query: LibraryQuery) => {
    if (disposed) return;
    const key = statsKey(profile, query);
    if (inflight.has(key)) {
      touch(key);
      return;
    }
    touch(key);

    if (profile === "summary") {
      const summary = entries.get(key);
      if (summary?.data != null && summary.version === version) return;
      const facets = entries.get(statsKey("facets", query));
      if (facets?.data != null) {
        deriveSummary(query);
        if (facets.version < version) {
          ensure("facets", query);
        }
        return;
      }
    }

    const entry = entries.get(key);
    if (entry?.data != null) {
      if (entry.version < version) schedule(profile, query, key);
      return;
    }
    void request(profile, query, key);
  };

  const fetch = async <P extends StatsProfile>(
    profile: P,
    query: LibraryQuery,
  ): Promise<StatsDataMap[P]> => {
    if (disposed) throw new Error("图库统计已关闭");
    const key = statsKey(profile, query);
    if (profile === "summary") {
      const derived = deriveSummary(query);
      if (derived != null) {
        const facets = entries.get(statsKey("facets", query));
        if (facets != null && facets.version < version) {
          await request("facets", query, statsKey("facets", query));
          const refreshed = deriveSummary(query);
          if (refreshed != null) return refreshed as StatsDataMap[P];
        }
        return derived as StatsDataMap[P];
      }
    }

    const entry = entries.get(key);
    if (entry?.data != null && entry.version === version) {
      return entry.data as StatsDataMap[P];
    }
    await request(profile, query, key);
    const result = entries.get(key);
    if (result?.data != null) return result.data as StatsDataMap[P];
    throw new Error(result?.error ?? "统计加载失败");
  };

  const invalidate = () => {
    if (disposed) return;
    version += 1;
    for (const key of inflight.keys()) staleAfterInflight.add(key);
    // 生成新的条目对象：in-place 改状态会让 useSyncExternalStore 的快照引用不变。
    for (const [key, entry] of entries) {
      if (entry.data != null && entry.status === "ready") {
        entries.set(key, { ...entry, status: "refreshing" });
      }
    }
    notify();
  };

  const retry = <P extends StatsProfile>(profile: P, query: LibraryQuery) => {
    if (disposed) return;
    const key = statsKey(profile, query);
    const timer = timers.get(key);
    if (timer != null) {
      clearTimeout(timer);
      timers.delete(key);
    }
    void request(profile, query, key);
  };

  const clear = () => {
    epoch += 1;
    version += 1;
    for (const timer of timers.values()) clearTimeout(timer);
    timers.clear();
    inflight.clear();
    inflightTokens.clear();
    staleAfterInflight.clear();
    generations.clear();
    entries.clear();
    notify();
  };

  return {
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    getVersion: () => version,
    get(profile, query) {
      return (entries.get(statsKey(profile, query)) ?? null) as StatsEntry<
        StatsDataMap[typeof profile]
      > | null;
    },
    ensure,
    fetch,
    invalidate,
    retry,
    clear,
    dispose() {
      if (disposed) return;
      disposed = true;
      clear();
      listeners.clear();
    },
  };
}

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import {
  statsKey,
  type StatsDataMap,
  type StatsStore,
} from "./statsStore";
import type { LibraryQuery, StatsProfile } from "./types";

export interface StatsResource<T> {
  data: T | null;
  status: "idle" | "loading" | "ready" | "refreshing" | "error";
  error: string | null;
  retry: () => void;
}

const StatsStoreContext = createContext<StatsStore | null>(null);

export function StatsStoreProvider({
  store,
  children,
}: {
  store: StatsStore;
  children: ReactNode;
}) {
  return (
    <StatsStoreContext.Provider value={store}>{children}</StatsStoreContext.Provider>
  );
}

export function useStatsStore(): StatsStore {
  const store = useContext(StatsStoreContext);
  if (!store) throw new Error("统计 store 尚未提供");
  return store;
}

export function useStats<P extends StatsProfile>(
  profile: P,
  query: LibraryQuery,
  enabled = true,
): StatsResource<StatsDataMap[P]> {
  const store = useStatsStore();
  const queryRef = useRef(query);
  queryRef.current = query;
  const key = statsKey(profile, query);
  const version = useSyncExternalStore(store.subscribe, store.getVersion);
  const entry = useSyncExternalStore(
    store.subscribe,
    () => store.get(profile, query),
  );

  useEffect(() => {
    if (!enabled) return;
    store.ensure(profile, queryRef.current);
  }, [store, profile, key, enabled, version]);

  const retry = useCallback(() => {
    store.retry(profile, queryRef.current);
  }, [store, profile]);

  return {
    data: entry?.data ?? null,
    status: entry?.status ?? "idle",
    error: entry?.error ?? null,
    retry,
  };
}

import { useCallback, useEffect, useRef, useState } from "react";
import { libraryApi } from "./api";
import { queryKey } from "./model";
import type { ImageRow, LibraryQuery, QueryCursor } from "./types";

/** 首屏状态：`error` 时整页显示错误与重试，`ready` 且无条目时显示空结果。 */
export type QueryStatus = "loading" | "ready" | "error";
/** 下一页状态；虚拟网格/列表底部据此显示载入中、失败重试或已到底。 */
export type MoreStatus = "idle" | "loading" | "error";

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "未知错误";
}

export interface ImageQueryState {
  items: ImageRow[];
  status: QueryStatus;
  error: string | null;
  moreStatus: MoreStatus;
  moreError: string | null;
  hasMore: boolean;
  loadMore: () => void;
  retry: () => void;
  retryMore: () => void;
}

/**
 * 键集分页控制器。查询条件变化时递增代次，过期的页结果直接丢弃，
 * 不覆盖新视图；`refreshToken` 用于导入完成等外部刷新。
 */
export function useImageQuery(query: LibraryQuery, refreshToken: number): ImageQueryState {
  const [items, setItems] = useState<ImageRow[]>([]);
  const [status, setStatus] = useState<QueryStatus>("loading");
  const [error, setError] = useState<string | null>(null);
  const [moreStatus, setMoreStatus] = useState<MoreStatus>("idle");
  const [moreError, setMoreError] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);

  const queryRef = useRef(query);
  queryRef.current = query;
  const generation = useRef(0);
  const inFlight = useRef(false);
  const cursorRef = useRef<QueryCursor | null>(null);

  const loadFirst = useCallback(async () => {
    const currentGeneration = generation.current + 1;
    generation.current = currentGeneration;
    inFlight.current = true;
    cursorRef.current = null;
    setItems([]);
    setHasMore(false);
    setStatus("loading");
    setMoreStatus("idle");
    setMoreError(null);
    setError(null);

    try {
      const page = await libraryApi.queryImages({ ...queryRef.current, cursor: null });
      if (currentGeneration !== generation.current) return;
      setItems(page.items);
      cursorRef.current = page.nextCursor;
      setHasMore(page.nextCursor !== null);
      setStatus("ready");
    } catch (cause) {
      if (currentGeneration !== generation.current) return;
      setStatus("error");
      setError(errorMessage(cause));
    } finally {
      if (currentGeneration === generation.current) {
        inFlight.current = false;
      }
    }
  }, []);

  const key = queryKey(query);

  useEffect(() => {
    void loadFirst();
  }, [key, refreshToken, loadFirst]);

  const loadMore = useCallback(async () => {
    if (inFlight.current) return;
    const cursor = cursorRef.current;
    if (!cursor) return;

    const currentGeneration = generation.current;
    inFlight.current = true;
    setMoreStatus("loading");
    setMoreError(null);

    try {
      const page = await libraryApi.queryImages({ ...queryRef.current, cursor });
      if (currentGeneration !== generation.current) return;
      if (page.items.length > 0) {
        setItems((previous) => {
          const known = new Set(previous.map((item) => item.id));
          return [...previous, ...page.items.filter((item) => !known.has(item.id))];
        });
      }
      cursorRef.current = page.nextCursor;
      setHasMore(page.nextCursor !== null);
      setMoreStatus("idle");
    } catch (cause) {
      if (currentGeneration !== generation.current) return;
      setMoreStatus("error");
      setMoreError(errorMessage(cause));
    } finally {
      if (currentGeneration === generation.current) {
        inFlight.current = false;
      }
    }
  }, []);

  return {
    items,
    status,
    error,
    moreStatus,
    moreError,
    hasMore,
    loadMore,
    retry: loadFirst,
    retryMore: loadMore,
  };
}

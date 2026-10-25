import { useCallback, useEffect, useRef, useState } from "react";
import { libraryApi } from "./api";
import { queryKey } from "./model";
import type { ImageRow, LibraryQuery, QueryCursor } from "./types";
import { errorMessage } from "../../shared/errors";

/** 首屏状态：`error` 时整页显示错误与重试，`ready` 且无条目时显示空结果。 */
export type QueryStatus = "loading" | "ready" | "error";
/** 下一页状态；虚拟网格/列表底部据此显示载入中、失败重试或已到底。 */
export type MoreStatus = "idle" | "loading" | "error";

export interface ImageQueryState {
  items: ImageRow[];
  status: QueryStatus;
  error: string | null;
  moreStatus: MoreStatus;
  moreError: string | null;
  hasMore: boolean;
  /** 当前 items 对应的查询键；切换筛选后到新结果返回前保持旧值或为空。 */
  loadedKey: string | null;
  loadMore: () => void;
  retry: () => void;
  retryMore: () => void;
  /** 就地替换已加载行（编辑保存后避免重置查询与滚动位置）。 */
  patchItem: (imageId: number, update: Partial<ImageRow>) => void;
  /** 从指定行所在位置开始取一页；用于筛选后定位原图。 */
  jumpTo: (anchor: Pick<ImageRow, "id">) => void;
}

/**
 * 分页控制器。单级索引排序用键集游标，多级/文件夹排序用 Rust 回传的有序 id 列表游标
 * （`pageToken`）；查询条件变化时递增代次，过期的页结果直接丢弃，不覆盖新视图；
 * `refreshToken` 用于导入完成等外部刷新。
 */
export function useImageQuery(query: LibraryQuery, refreshToken: number): ImageQueryState {
  const [items, setItems] = useState<ImageRow[]>([]);
  const [status, setStatus] = useState<QueryStatus>("loading");
  const [error, setError] = useState<string | null>(null);
  const [moreStatus, setMoreStatus] = useState<MoreStatus>("idle");
  const [moreError, setMoreError] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loadedKey, setLoadedKey] = useState<string | null>(null);

  const queryRef = useRef(query);
  queryRef.current = query;
  const generation = useRef(0);
  const inFlight = useRef(false);
  const cursorRef = useRef<QueryCursor | null>(null);
  const pageTokenRef = useRef<string | null>(null);

  const loadFirst = useCallback(async () => {
    const currentGeneration = generation.current + 1;
    generation.current = currentGeneration;
    inFlight.current = true;
    cursorRef.current = null;
    pageTokenRef.current = null;
    setItems([]);
    setHasMore(false);
    setLoadedKey(null);
    setStatus("loading");
    setMoreStatus("idle");
    setMoreError(null);
    setError(null);

    try {
      const requestKey = queryKey(queryRef.current);
      const page = await libraryApi.queryImages({
        ...queryRef.current,
        cursor: null,
        pageToken: null,
        anchorId: null,
      });
      if (currentGeneration !== generation.current) return;
      setItems(page.items);
      cursorRef.current = page.nextCursor;
      pageTokenRef.current = page.nextPageToken;
      setHasMore(page.nextCursor !== null || page.nextPageToken !== null);
      setLoadedKey(requestKey);
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
    const pageToken = pageTokenRef.current;
    const cursor = cursorRef.current;
    if (!pageToken && !cursor) return;

    const currentGeneration = generation.current;
    inFlight.current = true;
    setMoreStatus("loading");
    setMoreError(null);

    try {
      const page = await libraryApi.queryImages({
        ...queryRef.current,
        cursor: pageToken ? null : cursor,
        pageToken,
      });
      if (currentGeneration !== generation.current) return;
      if (page.items.length > 0) {
        setItems((previous) => {
          const known = new Set(previous.map((item) => item.id));
          return [...previous, ...page.items.filter((item) => !known.has(item.id))];
        });
      }
      cursorRef.current = page.nextCursor;
      pageTokenRef.current = page.nextPageToken;
      setHasMore(page.nextCursor !== null || page.nextPageToken !== null);
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

  const jumpTo = useCallback(async (anchor: Pick<ImageRow, "id">) => {
    const currentGeneration = generation.current + 1;
    generation.current = currentGeneration;
    inFlight.current = true;
    setStatus("loading");
    setError(null);
    setMoreStatus("idle");
    setMoreError(null);
    try {
      const requestKey = queryKey(queryRef.current);
      const page = await libraryApi.queryImages({
        ...queryRef.current,
        cursor: null,
        pageToken: null,
        anchorId: anchor.id,
      });
      if (currentGeneration !== generation.current) return;
      // 锚点页从目标图自身开始；找不到时返回空页，由调用方清空选择。
      setItems(page.items);
      cursorRef.current = page.nextCursor;
      pageTokenRef.current = page.nextPageToken;
      setHasMore(page.nextCursor !== null || page.nextPageToken !== null);
      setLoadedKey(requestKey);
      setStatus("ready");
    } catch (cause) {
      if (currentGeneration !== generation.current) return;
      setStatus("error");
      setError(errorMessage(cause));
    } finally {
      if (currentGeneration === generation.current) inFlight.current = false;
    }
  }, []);

  const patchItem = useCallback((imageId: number, update: Partial<ImageRow>) => {
    setItems((previous) => {
      const index = previous.findIndex((item) => item.id === imageId);
      if (index < 0) return previous;
      const next = previous.slice();
      next[index] = { ...next[index], ...update };
      return next;
    });
  }, []);

  return {
    items,
    status,
    error,
    moreStatus,
    moreError,
    hasMore,
    loadedKey,
    loadMore,
    retry: loadFirst,
    retryMore: loadMore,
    patchItem,
    jumpTo,
  };
}

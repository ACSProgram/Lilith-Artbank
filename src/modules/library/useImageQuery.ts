import { useCallback, useEffect, useRef, useState } from "react";
import { libraryApi } from "./api";
import { PAGE_SIZE, queryIdentityKey, queryKey, querySortKey } from "./model";
import type { ImageRow, LibraryQuery, QueryCursor } from "./types";
import { errorMessage } from "../../shared/errors";

/** 首屏状态：`error` 时整页显示错误与重试，`ready` 且无条目时显示空结果。 */
export type QueryStatus = "loading" | "ready" | "error";
/** 下一页状态；虚拟网格/列表底部据此显示载入中、失败重试或已到底。 */
export type MoreStatus = "idle" | "loading" | "error";

/** 一次刷新最多重取 20 页，避免深滚动后一次变化发起过多请求。 */
const MAX_REFRESH_PAGES = 20;

export interface ImageQueryState {
  items: ImageRow[];
  status: QueryStatus;
  error: string | null;
  /** 保留旧行刷新失败时的非阻塞错误；列表仍显示旧内容。 */
  refreshError: string | null;
  moreStatus: MoreStatus;
  moreError: string | null;
  hasMore: boolean;
  /** 当前 items 对应的查询键；切换筛选后到新结果返回前保持旧值或为空。 */
  loadedKey: string | null;
  /** 保留旧行刷新成功一次就递增，虚拟视图据此重新锚定活动图。 */
  replaceRevision: number;
  loadMore: () => void;
  retry: () => void;
  retryMore: () => void;
  /** 保留旧行，重新取已加载范围；用于排序变化与数据刷新。 */
  refreshKeepingRows: () => Promise<boolean>;
  /** 就地替换已加载行（编辑保存后避免重置查询与滚动位置）。 */
  patchItem: (imageId: number, update: Partial<ImageRow>) => void;
  patchItems: (patches: Array<{ id: number; update: Partial<ImageRow> }>) => void;
  /** 立即从已加载行里移除图片；随后通常还有一次保留旧行的刷新。 */
  removeItems: (imageIds: number[]) => void;
  clearRefreshError: () => void;
  /** 从指定行所在位置开始取一页；用于筛选后定位原图。 */
  jumpTo: (anchor: Pick<ImageRow, "id">) => void;
}

/**
 * 分页控制器。单级索引排序用键集游标，多级/文件夹排序用 Rust 回传的有序 id 列表游标
 * （`pageToken`）。查询身份变化才清空列表；排序与数据变化保留旧行，在新结果返回后
 * 一次性替换，不显示整页 loading、不回到顶部。
 */
export function useImageQuery(query: LibraryQuery): ImageQueryState {
  const [items, setItems] = useState<ImageRow[]>([]);
  const [status, setStatus] = useState<QueryStatus>("loading");
  const [error, setError] = useState<string | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [moreStatus, setMoreStatus] = useState<MoreStatus>("idle");
  const [moreError, setMoreError] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loadedKey, setLoadedKey] = useState<string | null>(null);
  const [replaceRevision, setReplaceRevision] = useState(0);

  const queryRef = useRef(query);
  queryRef.current = query;
  const itemsRef = useRef(items);
  itemsRef.current = items;
  const hasMoreRef = useRef(hasMore);
  hasMoreRef.current = hasMore;
  const generation = useRef(0);
  const inFlight = useRef(false);
  const cursorRef = useRef<QueryCursor | null>(null);
  const pageTokenRef = useRef<string | null>(null);

  const replaceItems = useCallback((next: ImageRow[]) => {
    itemsRef.current = next;
    setItems(next);
  }, []);

  const loadFirst = useCallback(async () => {
    const currentGeneration = generation.current + 1;
    generation.current = currentGeneration;
    inFlight.current = true;
    cursorRef.current = null;
    pageTokenRef.current = null;
    replaceItems([]);
    setHasMore(false);
    setLoadedKey(null);
    setStatus("loading");
    setMoreStatus("idle");
    setMoreError(null);
    setError(null);
    setRefreshError(null);

    try {
      const requestKey = queryKey(queryRef.current);
      const page = await libraryApi.queryImages({
        ...queryRef.current,
        cursor: null,
        pageToken: null,
        anchorId: null,
      });
      if (currentGeneration !== generation.current) return;
      replaceItems(page.items);
      cursorRef.current = page.nextCursor;
      pageTokenRef.current = page.nextPageToken;
      const nextHasMore = page.nextCursor !== null || page.nextPageToken !== null;
      hasMoreRef.current = nextHasMore;
      setHasMore(nextHasMore);
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
  }, [replaceItems]);

  const refreshKeepingRows = useCallback(async (): Promise<boolean> => {
    const previousCount = itemsRef.current.length;
    if (previousCount === 0) {
      await loadFirst();
      return true;
    }

    const currentGeneration = generation.current + 1;
    generation.current = currentGeneration;
    inFlight.current = true;
    setMoreStatus("idle");
    setMoreError(null);
    setRefreshError(null);

    const target = Math.min(previousCount, PAGE_SIZE * MAX_REFRESH_PAGES);
    const collected: ImageRow[] = [];
    const seen = new Set<number>();
    let cursor: QueryCursor | null = null;
    let pageToken: string | null = null;
    let pages = 0;

    try {
      // 页数上限既限制深滚动的请求数，也避免后端返回空页但仍有游标时死循环。
      while (collected.length < target && pages < MAX_REFRESH_PAGES) {
        pages += 1;
        const page = await libraryApi.queryImages({
          ...queryRef.current,
          cursor: pageToken ? null : cursor,
          pageToken,
          anchorId: null,
        });
        if (currentGeneration !== generation.current) return false;
        for (const item of page.items) {
          if (seen.has(item.id)) continue;
          seen.add(item.id);
          collected.push(item);
        }
        cursor = page.nextCursor;
        pageToken = page.nextPageToken;
        if (cursor === null && pageToken === null) break;
      }
      if (currentGeneration !== generation.current) return false;

      const nextItems = collected.slice(0, target);
      cursorRef.current = cursor;
      pageTokenRef.current = pageToken;
      const nextHasMore = cursor !== null || pageToken !== null;
      hasMoreRef.current = nextHasMore;
      setHasMore(nextHasMore);
      replaceItems(nextItems);
      setLoadedKey(queryKey(queryRef.current));
      setStatus("ready");
      setReplaceRevision((value) => value + 1);
      return true;
    } catch (cause) {
      if (currentGeneration !== generation.current) return false;
      setRefreshError(errorMessage(cause));
      setStatus(itemsRef.current.length > 0 ? "ready" : "error");
      return false;
    } finally {
      if (currentGeneration === generation.current) {
        inFlight.current = false;
      }
    }
  }, [loadFirst, replaceItems]);

  const patchItems = useCallback(
    (patches: Array<{ id: number; update: Partial<ImageRow> }>) => {
      if (patches.length === 0) return;
      const byId = new Map(patches.map((patch) => [patch.id, patch.update]));
      const next = itemsRef.current.map((item) => {
        const update = byId.get(item.id);
        return update ? { ...item, ...update } : item;
      });
      replaceItems(next);
    },
    [replaceItems],
  );

  const patchItem = useCallback(
    (imageId: number, update: Partial<ImageRow>) => {
      patchItems([{ id: imageId, update }]);
    },
    [patchItems],
  );

  const removeItems = useCallback(
    (imageIds: number[]) => {
      if (imageIds.length === 0) return;
      const removed = new Set(imageIds);
      replaceItems(itemsRef.current.filter((item) => !removed.has(item.id)));
    },
    [replaceItems],
  );

  const identityKey = queryIdentityKey(query);
  const sortKey = querySortKey(query);
  const previousQueryRef = useRef<{ identity: string; sort: string } | null>(null);

  useEffect(() => {
    const previous = previousQueryRef.current;
    previousQueryRef.current = { identity: identityKey, sort: sortKey };
    if (!previous || previous.identity !== identityKey) {
      void loadFirst();
      return;
    }
    if (previous.sort !== sortKey) {
      void refreshKeepingRows();
    }
  }, [identityKey, loadFirst, refreshKeepingRows, sortKey]);

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
        const known = new Set(itemsRef.current.map((item) => item.id));
        const appended = page.items.filter((item) => !known.has(item.id));
        if (appended.length > 0) {
          replaceItems([...itemsRef.current, ...appended]);
        }
      }
      cursorRef.current = page.nextCursor;
      pageTokenRef.current = page.nextPageToken;
      const nextHasMore = page.nextCursor !== null || page.nextPageToken !== null;
      hasMoreRef.current = nextHasMore;
      setHasMore(nextHasMore);
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
  }, [replaceItems]);

  const jumpTo = useCallback(async (anchor: Pick<ImageRow, "id">) => {
    const currentGeneration = generation.current + 1;
    generation.current = currentGeneration;
    inFlight.current = true;
    setStatus("loading");
    setError(null);
    setRefreshError(null);
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
      replaceItems(page.items);
      cursorRef.current = page.nextCursor;
      pageTokenRef.current = page.nextPageToken;
      const nextHasMore = page.nextCursor !== null || page.nextPageToken !== null;
      hasMoreRef.current = nextHasMore;
      setHasMore(nextHasMore);
      setLoadedKey(requestKey);
      setStatus("ready");
    } catch (cause) {
      if (currentGeneration !== generation.current) return;
      setStatus("error");
      setError(errorMessage(cause));
    } finally {
      if (currentGeneration === generation.current) inFlight.current = false;
    }
  }, [replaceItems]);

  const clearRefreshError = useCallback(() => setRefreshError(null), []);

  return {
    items,
    status,
    error,
    refreshError,
    moreStatus,
    moreError,
    hasMore,
    loadedKey,
    replaceRevision,
    loadMore,
    retry: loadFirst,
    retryMore: loadMore,
    refreshKeepingRows,
    patchItem,
    patchItems,
    removeItems,
    clearRefreshError,
    jumpTo,
  };
}

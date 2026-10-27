import { useCallback, useMemo, useRef, useState } from "react";
import {
  buildQuery,
  defaultBrowseState,
  queryIdentityKey,
  type BrowseState,
} from "./model";
import type { SortSpec } from "./types";

export interface LibraryBrowseController {
  browse: BrowseState;
  applyBrowse: (next: BrowseState, pushHistory?: boolean) => void;
  canGoBack: boolean;
  goBack: () => void;
}

/** 树与网格共享的浏览状态；历史仍只记录视图/搜索/筛选变化。 */
export function useLibraryBrowse(initialSort?: SortSpec[]): LibraryBrowseController {
  const [browse, setBrowse] = useState<BrowseState>(() => defaultBrowseState(initialSort));
  const [history, setHistory] = useState<BrowseState[]>([]);
  const browseRef = useRef(browse);
  browseRef.current = browse;
  const historyRef = useRef(history);
  historyRef.current = history;

  const applyBrowse = useCallback((next: BrowseState, pushHistory = true) => {
    const previous = browseRef.current;
    if (pushHistory && queryIdentityKey(buildQuery(previous)) !== queryIdentityKey(buildQuery(next))) {
      setHistory((value) => [...value.slice(-29), previous]);
    }
    browseRef.current = next;
    setBrowse(next);
  }, []);

  const goBack = useCallback(() => {
    const previous = historyRef.current;
    if (previous.length === 0) return;
    const next = previous[previous.length - 1];
    setHistory(previous.slice(0, -1));
    browseRef.current = next;
    setBrowse(next);
  }, []);

  return useMemo(
    () => ({ browse, applyBrowse, canGoBack: history.length > 0, goBack }),
    [browse, applyBrowse, history.length, goBack],
  );
}

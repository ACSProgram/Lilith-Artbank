import { useCallback, useMemo, useRef, useState } from "react";
import { sameGeometry, type GeometryState } from "./model";

export const MAX_HISTORY_STEPS = 50;
/** 同一滑块连续调整在此窗口内合并成一步。 */
const MERGE_WINDOW_MS = 600;

interface Entry {
  state: GeometryState;
  /** 产生这个状态的操作名；初始状态为 null。 */
  label: string | null;
}

interface HistoryState {
  entries: Entry[];
  index: number;
}

export interface EditHistory {
  geometry: GeometryState | null;
  canUndo: boolean;
  canRedo: boolean;
  undoLabel: string | null;
  redoLabel: string | null;
  reset: (geometry: GeometryState | null) => void;
  commit: (
    next: GeometryState,
    options: { label: string | null; mergeKey?: string | null },
  ) => void;
  /** 返回撤销后的状态，供调用方同步裁剪草稿等界面状态。 */
  undo: () => GeometryState | null;
  redo: () => GeometryState | null;
}

/** 会话内的编辑撤销栈；切换图片时整体重置，保存成功后由重新载入重置。 */
export function useEditHistory(): EditHistory {
  const [state, setState] = useState<HistoryState>({ entries: [], index: 0 });
  const stateRef = useRef(state);
  stateRef.current = state;
  const lastMerge = useRef<{ key: string; at: number } | null>(null);

  const reset = useCallback((geometry: GeometryState | null) => {
    lastMerge.current = null;
    setState(
      geometry
        ? { entries: [{ state: geometry, label: null }], index: 0 }
        : { entries: [], index: 0 },
    );
  }, []);

  const commit = useCallback(
    (next: GeometryState, options: { label: string | null; mergeKey?: string | null }) => {
      const previous = stateRef.current;
      const current = previous.entries[previous.index];
      if (!current || sameGeometry(current.state, next)) return;
      const now = Date.now();
      const mergeKey = options.mergeKey ?? null;
      const merge =
        mergeKey != null &&
        lastMerge.current?.key === mergeKey &&
        now - lastMerge.current.at < MERGE_WINDOW_MS &&
        previous.index > 0;
      lastMerge.current = mergeKey == null ? null : { key: mergeKey, at: now };

      setState(() => {
        if (merge) {
          const entries = previous.entries.slice();
          entries[previous.index] = { state: next, label: options.label };
          return { entries, index: previous.index };
        }
        const entries = [
          ...previous.entries.slice(0, previous.index + 1),
          { state: next, label: options.label },
        ];
        // 初始状态也占一个槽位，因此最多保留 MAX_HISTORY_STEPS 次可撤销操作。
        while (entries.length > MAX_HISTORY_STEPS + 1) entries.shift();
        return { entries, index: entries.length - 1 };
      });
    },
    [],
  );

  const undo = useCallback(() => {
    const previous = stateRef.current;
    if (previous.index <= 0) return null;
    const next = { entries: previous.entries, index: previous.index - 1 };
    stateRef.current = next;
    setState(next);
    lastMerge.current = null;
    return next.entries[next.index].state;
  }, []);

  const redo = useCallback(() => {
    const previous = stateRef.current;
    if (previous.index >= previous.entries.length - 1) return null;
    const next = { entries: previous.entries, index: previous.index + 1 };
    stateRef.current = next;
    setState(next);
    lastMerge.current = null;
    return next.entries[next.index].state;
  }, []);

  const geometry = state.entries[state.index]?.state ?? null;
  return useMemo(
    () => ({
      geometry,
      canUndo: state.index > 0,
      canRedo: state.index < state.entries.length - 1,
      undoLabel: state.entries[state.index]?.label ?? null,
      redoLabel: state.entries[state.index + 1]?.label ?? null,
      reset,
      commit,
      undo,
      redo,
    }),
    [commit, geometry, redo, reset, state],
  );
}

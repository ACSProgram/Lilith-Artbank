import { useCallback, useEffect, useRef, useState } from "react";
import { libraryApi } from "./api";
import type { SelectionTags } from "./types";
import { errorMessage } from "../../shared/errors";

/** 去抖窗口：连点选择只发一次查询。 */
const DEBOUNCE_MS = 120;

export interface SelectionTagsState {
  /** `null` 表示当前选择还没有结果：首次加载中，或选择刚变、旧结果已丢弃。 */
  tags: SelectionTags | null;
  /** 重新计算中。已有结果时保留旧列表，调用方只需显示轻量加载指示。 */
  loading: boolean;
  error: string | null;
  retry: () => void;
  /** 本地抹掉一个标签，供删除成功后立刻更新候选；下一次重算会覆盖。 */
  forget: (name: string) => void;
}

/**
 * 读取选中集合内各标签的拥有数，供批量侧栏的移除候选使用。
 * 只在选择变化或 `version`（标签关系/标签目录变化）时重算；评分、描述、文件夹变化不触发。
 */
export function useSelectionTags(imageIds: number[], version: number): SelectionTagsState {
  const [tags, setTags] = useState<SelectionTags | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [retryToken, setRetryToken] = useState(0);
  const generation = useRef(0);
  const previousIds = useRef<number[] | null>(null);

  useEffect(() => {
    // 选择换了就丢掉上一批的候选，避免把旧标签算到新选择上。
    if (previousIds.current !== imageIds) {
      previousIds.current = imageIds;
      setTags(null);
    }
    if (imageIds.length === 0) {
      setTags({ total: 0, tags: [] });
      setLoading(false);
      setError(null);
      return;
    }
    let disposed = false;
    const current = ++generation.current;
    setLoading(true);
    setError(null);
    const timer = window.setTimeout(() => {
      void libraryApi
        .getSelectionTags(imageIds)
        .then((result) => {
          if (disposed || current !== generation.current) return;
          setTags(result);
        })
        .catch((cause: unknown) => {
          if (disposed || current !== generation.current) return;
          setError(errorMessage(cause));
        })
        .finally(() => {
          if (disposed || current !== generation.current) return;
          setLoading(false);
        });
    }, DEBOUNCE_MS);
    return () => {
      disposed = true;
      window.clearTimeout(timer);
    };
  }, [imageIds, version, retryToken]);

  const retry = useCallback(() => setRetryToken((value) => value + 1), []);
  const forget = useCallback((name: string) => {
    setTags((previous) =>
      previous
        ? { ...previous, tags: previous.tags.filter((tag) => tag.name !== name) }
        : previous,
    );
  }, []);

  return { tags, loading, error, retry, forget };
}

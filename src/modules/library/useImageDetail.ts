import { useCallback, useEffect, useState } from "react";
import { errorMessage } from "../../shared/errors";
import { libraryApi } from "./api";
import type { ImageDetails } from "./types";

/**
 * 按 imageId/version 拉取单张详情，并提供带身份守卫的就地更新。
 * 切图清空、版本刷新保留旧内容、写入结果只落到同一张图上，全部在这里统一处理。
 */
export function useImageDetail(imageId: number, version: number) {
  const [detail, setDetail] = useState<ImageDetails | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [thumbFailed, setThumbFailed] = useState(false);
  const [folderDraft, setFolderDraft] = useState("");

  /**
   * 只对“这张图”应用就地更新：切图后上一张的写入结果不能落到新图上。
   * 详情被清空时（还没读回来）直接丢弃。
   */
  const patchDetail = useCallback((id: number, update: Partial<ImageDetails>) => {
    setDetail((previous) =>
      previous && previous.id === id ? { ...previous, ...update } : previous,
    );
  }, []);

  useEffect(() => {
    let disposed = false;
    // 切图先清空上一张的详情：标题与操作按钮都用当前 imageId，留着旧内容会张冠李戴。
    // 同一张图的版本刷新（写入后重取）保留旧内容，避免每次改评分都闪一下加载态。
    setDetail((previous) => (previous && previous.id === imageId ? previous : null));
    setLoading(true);
    setLoadError(null);
    void libraryApi
      .getImageDetail(imageId)
      .then((next) => {
        if (disposed) return;
        if (next.id !== imageId) return;
        setDetail(next);
        setFolderDraft(next.folderPath);
        setThumbFailed(false);
      })
      .catch((cause: unknown) => {
        if (disposed) return;
        setLoadError(errorMessage(cause));
      })
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, [imageId, version]);

  return {
    detail,
    loading,
    loadError,
    folderDraft,
    setFolderDraft,
    thumbFailed,
    setThumbFailed,
    patchDetail,
  };
}

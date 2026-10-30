import {
  useEffect,
  useRef,
  useState,
  type Dispatch,
  type RefObject,
  type SetStateAction,
} from "react";
import { errorMessage } from "../../shared/errors";
import type { ViewerSessionItem } from "../../shared/viewerSession";
import { viewerApi } from "./api";
import { rescaleZoom } from "./model";
import type { PoseData, ViewerFallback, ViewerSource } from "./types";

type LoadState =
  | { imageId: number; status: "loading" }
  | { imageId: number; status: "error"; error: string }
  | { imageId: number; status: "ready"; source: ViewerSource };

interface MediaState {
  imageId: number;
  loaded: boolean;
  failed: boolean;
  thumbFailed: boolean;
}

function initialMedia(imageId: number): MediaState {
  return { imageId, loaded: false, failed: false, thumbFailed: false };
}

function fallbackFor(
  imageId: number,
  item: ViewerSessionItem | undefined,
): ViewerFallback {
  if (item) {
    return {
      width: item.width,
      height: item.height,
      contentHash: item.contentHash,
      fileExt: item.fileExt,
      format: item.format,
      fileSize: item.fileSize,
      hasAlpha: item.hasAlpha,
      deletedAt: item.deletedAt,
      thumbVersion: item.thumbVersion,
    };
  }
  return {
    width: 1600,
    height: 1200,
    contentHash: `mock-${imageId}`,
    fileExt: "png",
    format: "png",
    fileSize: 0,
    hasAlpha: false,
    deletedAt: null,
    thumbVersion: 1,
  };
}

interface UseViewerSourceOptions {
  initialId: number;
  activeId: number;
  contentKey: string;
  items: readonly ViewerSessionItem[];
  fitActiveRef: RefObject<boolean>;
  setZoom: Dispatch<SetStateAction<number>>;
  setPose: Dispatch<SetStateAction<PoseData | null>>;
  /** 切图时同步复位查看器状态（姿势、裁剪、拖拽与撤销栈）。 */
  onSwitch: () => void;
  /** 源图就绪：把撤销栈重置到初始几何。 */
  onReady: (source: ViewerSource) => void;
}

/**
 * 当前图的源图加载、渐进层（缩略图到原图）、媒体加载状态与失败重试。
 * 过期请求直接丢弃；大图预览由 Rust 生成并缓存。
 */
export function useViewerSource({
  initialId,
  activeId,
  contentKey,
  items,
  fitActiveRef,
  setZoom,
  setPose,
  onSwitch,
  onReady,
}: UseViewerSourceOptions) {
  const itemsRef = useRef(items);
  itemsRef.current = items;

  const [load, setLoad] = useState<LoadState>({
    imageId: initialId,
    status: "loading",
  });
  const activeLoad =
    load.imageId === activeId
      ? load
      : ({ imageId: activeId, status: "loading" } as const);
  const source = activeLoad.status === "ready" ? activeLoad.source : null;
  const sourceError = activeLoad.status === "error" ? activeLoad.error : null;

  const [media, setMedia] = useState<MediaState>(() => initialMedia(initialId));
  const activeMedia =
    media.imageId === activeId ? media : initialMedia(activeId);
  // 回调里只按 imageId 归属，不展开渲染时的 activeMedia，避免丢掉同帧之后的复位。
  const patchMedia = (imageId: number, patch: Partial<MediaState>) => {
    setMedia((current) => ({
      ...(current.imageId === imageId ? current : initialMedia(imageId)),
      ...patch,
    }));
  };

  const [retry, setRetry] = useState(0);
  const [reloadToken, setReloadToken] = useState(0);

  const sourceRef = useRef(source);
  sourceRef.current = source;

  // `contentKey` 在体内不用到，只作为重取条件：内容被替换后图片 id 不变，只按 id 判断会停在旧图。
  useEffect(() => {
    let disposed = false;
    const fromPlaceholder = sourceRef.current == null;
    const placeholderWidth = fromPlaceholder
      ? (itemsRef.current.find((item) => item.id === activeId)?.width ?? 0)
      : 0;

    setLoad({ imageId: activeId, status: "loading" });
    // 只在切到另一张图时复位；同一张图的复位由重试与保存入口先行完成。
    setMedia((current) =>
      current.imageId === activeId ? current : initialMedia(activeId),
    );
    onSwitch();

    viewerApi
      .getViewerSource(
        activeId,
        fallbackFor(
          activeId,
          itemsRef.current.find((item) => item.id === activeId),
        ),
      )
      .then((next) => {
        if (disposed) return;
        if (!fitActiveRef.current && placeholderWidth > 0) {
          setZoom((value) => rescaleZoom(value, placeholderWidth, next.displayWidth));
        }
        setLoad({ imageId: activeId, status: "ready", source: next });
        onReady(next);
      })
      .catch((cause: unknown) => {
        if (disposed) return;
        setLoad({ imageId: activeId, status: "error", error: errorMessage(cause) });
      });

    viewerApi
      .getImagePose(activeId)
      .then((next) => {
        if (!disposed) setPose(next);
      })
      .catch(() => {
        if (!disposed) setPose(null);
      });

    return () => {
      disposed = true;
    };
  }, [
    activeId,
    contentKey,
    fitActiveRef,
    onReady,
    onSwitch,
    reloadToken,
    retry,
    setZoom,
    setPose,
  ]);

  const resetMedia = (imageId: number) => {
    setMedia(initialMedia(imageId));
  };

  const reload = () => {
    setReloadToken((value) => value + 1);
  };

  const retryLoad = () => {
    setLoad({ imageId: activeId, status: "loading" });
    setMedia(initialMedia(activeId));
    setRetry((value) => value + 1);
  };

  return {
    source,
    sourceError,
    activeLoad,
    activeMedia,
    patchMedia,
    retry,
    retryLoad,
    resetMedia,
    reload,
  };
}

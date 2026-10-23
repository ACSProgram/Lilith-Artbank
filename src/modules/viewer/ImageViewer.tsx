import {
  useCallback,
  useEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import {
  AlertTriangle,
  ChevronLeft,
  ChevronRight,
  Copy,
  Maximize,
  Minimize,
  PanelRight,
  PersonStanding,
  RefreshCw,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide-react";
import { useNotices } from "../../app/noticeContext";
import { Button, IconButton } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { errorMessage } from "../../shared/errors";
import type { ViewerSession, ViewerSessionItem } from "../../shared/viewerSession";
import { viewerApi, withRetry } from "./api";
import {
  baseOutputSize,
  clampCrop,
  cropToOriginal,
  fitZoom,
  formatBytes,
  formatDimensions,
  hasGeometryEdits,
  hasPixelChanges,
  initialGeometry,
  outputSize,
  rectBetween,
  transformedSize,
  type GeometryState,
} from "./model";
import { hasVisiblePose, PoseOverlay } from "./PoseOverlay";
import type { CropRect, PoseData, ViewerFallback, ViewerSource } from "./types";
import { ViewerTools } from "./ViewerTools";

interface ImageViewerProps {
  session: ViewerSession;
  onClose: () => void;
  sidebar?: ReactNode;
  sidebarOpen: boolean;
  onToggleSidebar: () => void;
  onActiveChange?: (imageId: number) => void;
  onSaved?: (imageId: number) => void;
  onGeneratePose?: () => Promise<PoseData>;
}

type SourceStatus = "loading" | "ready" | "error";

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

export function ImageViewer({
  session,
  onClose,
  sidebar,
  sidebarOpen,
  onToggleSidebar,
  onActiveChange,
  onSaved,
  onGeneratePose,
}: ImageViewerProps) {
  const { notify } = useNotices();
  const items = useSyncExternalStore(session.subscribe, session.getItems, session.getItems);
  const itemsRef = useRef(items);
  itemsRef.current = items;

  const [activeId, setActiveId] = useState(session.initialId);
  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;
  const [source, setSource] = useState<ViewerSource | null>(null);
  const [sourceStatus, setSourceStatus] = useState<SourceStatus>("loading");
  const [sourceError, setSourceError] = useState<string | null>(null);
  const [geometry, setGeometry] = useState<GeometryState | null>(null);
  const [pose, setPose] = useState<PoseData | null>(null);
  const [poseOpen, setPoseOpen] = useState(false);
  const [poseBusy, setPoseBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);
  const [retry, setRetry] = useState(0);
  const [imageLoaded, setImageLoaded] = useState(false);
  const [imageFailed, setImageFailed] = useState(false);
  const [saving, setSaving] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);

  const [zoom, setZoom] = useState(1);
  const [fitActive, setFitActive] = useState(true);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const [viewport, setViewport] = useState({ width: 0, height: 0 });
  const [dragging, setDragging] = useState(false);

  const [cropMode, setCropMode] = useState(false);
  const [cropDraft, setCropDraft] = useState<CropRect | null>(null);
  const [cropDragging, setCropDragging] = useState(false);
  const [pendingStep, setPendingStep] = useState(0);
  const cropStart = useRef<{ x: number; y: number } | null>(null);
  const dragRef = useRef<{ x: number; y: number; panX: number; panY: number } | null>(null);

  const overlayRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLDivElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const sourceRef = useRef(source);
  sourceRef.current = source;
  const zoomRef = useRef(zoom);
  zoomRef.current = zoom;
  const panRef = useRef(pan);
  panRef.current = pan;

  const index = items.findIndex((item) => item.id === activeId);

  const hasChanges = source && geometry ? hasPixelChanges(source, geometry) : false;
  const geometryEdits = source && geometry ? hasGeometryEdits(source, geometry) : false;
  const hasPose = hasVisiblePose(pose);

  // 源图加载：过期请求直接丢弃；大图预览由 Rust 生成并缓存。
  useEffect(() => {
    let disposed = false;
    setSourceStatus("loading");
    setSourceError(null);
    setImageLoaded(false);
    setImageFailed(false);
    setPose(null);
    setPoseOpen(false);
    setCropMode(false);
    setCropDraft(null);
    setPendingStep(0);
    setFitActive(true);
    setPan({ x: 0, y: 0 });
    setSource(null);
    setGeometry(null);

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
        setSource(next);
        setGeometry(initialGeometry(next));
        setSourceStatus("ready");
      })
      .catch((cause: unknown) => {
        if (disposed) return;
        setSourceStatus("error");
        setSourceError(errorMessage(cause));
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
  }, [activeId, reloadToken, retry]);

  useEffect(() => {
    session.onActiveChange(activeId);
    onActiveChange?.(activeId);
  }, [activeId, onActiveChange, session]);

  // 把焦点从网格移进查看器：网格的键盘处理挂在容器上，焦点留在网格会让
  // ←/→、Esc 同时作用于网格和查看器。
  useEffect(() => {
    overlayRef.current?.focus();
  }, []);

  // 视口尺寸用于“适应窗口”。
  useEffect(() => {
    const element = canvasRef.current;
    if (!element) return;
    const observer = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect;
      if (rect) setViewport({ width: rect.width, height: rect.height });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const fit = source ? fitZoom(source, geometry?.rotate ?? 0, viewport.width, viewport.height) : 1;

  useEffect(() => {
    if (fitActive && source) {
      setZoom(fit);
      setPan({ x: 0, y: 0 });
    }
  }, [fit, fitActive, source]);

  const zoomBy = useCallback((factor: number) => {
    setFitActive(false);
    setZoom((previous) => Math.min(8, Math.max(0.02, previous * factor)));
  }, []);

  const fitView = useCallback(() => {
    setFitActive(true);
    setPan({ x: 0, y: 0 });
  }, []);

  const actualSize = useCallback(() => {
    setFitActive(false);
    setZoom(1);
    setPan({ x: 0, y: 0 });
  }, []);

  // 滚轮缩放以光标为中心；非 passive 监听才能阻止 WebView 的页面滚动。
  useEffect(() => {
    const element = canvasRef.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      if (!sourceRef.current) return;
      event.preventDefault();
      const factor = Math.exp(-event.deltaY * 0.0015);
      const previous = zoomRef.current;
      const next = Math.min(8, Math.max(0.02, previous * factor));
      if (next === previous) return;
      const rect = element.getBoundingClientRect();
      const cursorX = event.clientX - rect.left - rect.width / 2 - panRef.current.x;
      const cursorY = event.clientY - rect.top - rect.height / 2 - panRef.current.y;
      const ratio = next / previous;
      setPan({
        x: panRef.current.x - cursorX * (ratio - 1),
        y: panRef.current.y - cursorY * (ratio - 1),
      });
      setZoom(next);
      setFitActive(false);
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, []);

  const step = useCallback(
    (delta: number) => {
      if (index < 0) return;
      const target = index + delta;
      if (target >= 0 && target < items.length) {
        setActiveId(items[target].id);
        setPendingStep(0);
        return;
      }
      if (delta > 0 && session.hasMore()) {
        setPendingStep(1);
        session.loadMore();
      }
    },
    [index, items, session],
  );

  useEffect(() => {
    if (pendingStep === 0) return;
    const current = items.findIndex((item) => item.id === activeId);
    const target = current + pendingStep;
    if (target >= 0 && target < items.length) {
      setPendingStep(0);
      setActiveId(items[target].id);
    } else if (!session.hasMore()) {
      setPendingStep(0);
    }
  }, [activeId, items, pendingStep, session]);

  const toggleFullscreen = useCallback(async () => {
    try {
      const next = await viewerApi.toggleFullscreen();
      setFullscreen(next);
    } catch (cause) {
      notify("切换全屏失败", "error", errorMessage(cause));
    }
  }, [notify]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (document.querySelector(".modal-backdrop")) return;
      const target = event.target as HTMLElement | null;
      if (target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) return;

      switch (event.key) {
        case "Escape":
          event.preventDefault();
          if (cropMode) {
            setCropMode(false);
            setCropDraft(geometry?.crop ?? null);
          } else if (poseOpen) {
            setPoseOpen(false);
          } else {
            onClose();
          }
          break;
        case "ArrowLeft":
          event.preventDefault();
          step(-1);
          break;
        case "ArrowRight":
          event.preventDefault();
          step(1);
          break;
        case "f":
        case "F":
          event.preventDefault();
          void toggleFullscreen();
          break;
        case "0":
          event.preventDefault();
          fitView();
          break;
        case "1":
          event.preventDefault();
          actualSize();
          break;
        case "+":
        case "=":
          event.preventDefault();
          zoomBy(1.25);
          break;
        case "-":
          event.preventDefault();
          zoomBy(0.8);
          break;
        default:
          break;
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [
    cropMode,
    geometry?.crop,
    onClose,
    poseOpen,
    step,
    toggleFullscreen,
    actualSize,
    fitView,
    zoomBy,
  ]);

  const rotateBy = (delta: number) => {
    setGeometry((previous) =>
      previous
        ? {
            ...previous,
            rotate: ((previous.rotate + delta) % 4 + 4) % 4,
            crop: null,
            resize: null,
          }
        : previous,
    );
    setCropDraft(null);
    setCropMode(false);
    setFitActive(true);
    setPan({ x: 0, y: 0 });
  };

  const flip = (axis: "horizontal" | "vertical") => {
    setGeometry((previous) =>
      previous
        ? {
            ...previous,
            flipHorizontal:
              axis === "horizontal" ? !previous.flipHorizontal : previous.flipHorizontal,
            flipVertical:
              axis === "vertical" ? !previous.flipVertical : previous.flipVertical,
            crop: null,
            resize: null,
          }
        : previous,
    );
    setCropDraft(null);
    setCropMode(false);
  };

  const enterCrop = () => {
    if (!geometry) return;
    setCropDraft(geometry.crop);
    setCropMode(true);
  };

  const cancelCrop = () => {
    setCropMode(false);
    setCropDraft(geometry?.crop ?? null);
  };

  const applyCrop = () => {
    if (!source || !geometry || !cropDraft) return;
    const bounds = transformedSize(source.displayWidth, source.displayHeight, geometry.rotate);
    const crop = clampCrop(cropDraft, bounds.width, bounds.height);
    setGeometry((previous) => (previous ? { ...previous, crop, resize: null } : previous));
    setCropDraft(null);
    setCropMode(false);
  };

  const clearCrop = () => {
    setGeometry((previous) =>
      previous ? { ...previous, crop: null, resize: null } : previous,
    );
    setCropDraft(null);
    setCropMode(false);
  };

  const resetGeometry = () => {
    if (!source) return;
    setGeometry(initialGeometry(source));
    setCropDraft(null);
    setCropMode(false);
    setFitActive(true);
    setPan({ x: 0, y: 0 });
  };

  const save = async () => {
    if (!source || !geometry || saving || !hasChanges || cropMode) return;
    setSaving(true);
    try {
      const crop = geometry.crop
        ? cropToOriginal(geometry.crop, geometry.rotate, source)
        : null;
      const base = baseOutputSize(source, geometry);
      const resize =
        geometry.resize &&
        (geometry.resize.width !== base.width || geometry.resize.height !== base.height)
          ? geometry.resize
          : null;
      const result = await viewerApi.saveImageEdit({
        imageId: activeId,
        rotate: geometry.rotate,
        flipHorizontal: geometry.flipHorizontal,
        flipVertical: geometry.flipVertical,
        crop,
        resize,
        outputFormat: geometry.outputFormat,
        quality: geometry.quality,
      });
      if (!result.changed) {
        notify("内容没有变化", "info");
      } else {
        notify(
          "编辑已保存",
          "success",
          `新内容 ${formatDimensions(result.width, result.height)} · ${formatBytes(result.fileSize)}`,
        );
      }
      setReloadToken((value) => value + 1);
      session.onSaved(activeId);
      onSaved?.(activeId);
    } catch (cause) {
      notify("保存失败", "error", errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };

  const togglePose = async () => {
    if (hasPose) {
      setPoseOpen((value) => !value);
      return;
    }
    if (!onGeneratePose || poseBusy) return;
    const target = activeId;
    setPoseBusy(true);
    try {
      const next = await onGeneratePose();
      if (activeIdRef.current !== target) return;
      setPose(next);
      if (hasVisiblePose(next)) {
        setPoseOpen(true);
        notify("姿势骨架已生成", "success");
      } else {
        notify("没有检测到人体姿势", "info", "图片中可能没有清晰可见的人物。");
      }
    } catch (cause) {
      notify("姿势识别失败", "error", errorMessage(cause));
    } finally {
      setPoseBusy(false);
    }
  };

  const copyImage = async () => {
    try {
      await viewerApi.copyImage(activeId);
      notify("图片已复制到剪贴板", "success");
    } catch (cause) {
      notify("复制图片失败", "error", errorMessage(cause));
    }
  };

  const stagePoint = (event: ReactPointerEvent) => {
    const rect = stageRef.current?.getBoundingClientRect();
    if (!rect) return null;
    return {
      x: (event.clientX - rect.left) / zoom,
      y: (event.clientY - rect.top) / zoom,
    };
  };

  const handlePointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!source) return;
    if (cropMode && event.button === 0) {
      const point = stagePoint(event);
      if (!point) return;
      event.preventDefault();
      event.currentTarget.setPointerCapture(event.pointerId);
      cropStart.current = point;
      setCropDraft(null);
      setCropDragging(true);
      return;
    }
    if (event.button !== 0 && event.button !== 1) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    dragRef.current = {
      x: event.clientX,
      y: event.clientY,
      panX: pan.x,
      panY: pan.y,
    };
    setDragging(true);
  };

  const handlePointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (cropStart.current && source) {
      const point = stagePoint(event);
      if (!point) return;
      const bounds = transformedSize(
        source.displayWidth,
        source.displayHeight,
        geometry?.rotate ?? 0,
      );
      setCropDraft(
        rectBetween(cropStart.current, point, bounds.width, bounds.height),
      );
      return;
    }
    if (!dragRef.current) return;
    setPan({
      x: dragRef.current.panX + event.clientX - dragRef.current.x,
      y: dragRef.current.panY + event.clientY - dragRef.current.y,
    });
  };

  const endPointer = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    cropStart.current = null;
    setCropDragging(false);
    dragRef.current = null;
    setDragging(false);
  };

  const rotate = geometry?.rotate ?? 0;
  const flipH = geometry?.flipHorizontal ?? false;
  const flipV = geometry?.flipVertical ?? false;
  const fullSize = source
    ? transformedSize(source.displayWidth, source.displayHeight, rotate)
    : { width: 0, height: 0 };
  const appliedCrop = !cropMode ? geometry?.crop ?? null : null;
  const clipWidth = (appliedCrop ? appliedCrop.width : fullSize.width) * zoom;
  const clipHeight = (appliedCrop ? appliedCrop.height : fullSize.height) * zoom;
  const overlayRect = cropMode ? cropDraft ?? geometry?.crop ?? null : null;
  const overlayOriginal =
    overlayRect && source ? cropToOriginal(overlayRect, rotate, source) : null;
  const imageTransform = `translate(-50%, -50%) rotate(${rotate * 90}deg) scaleX(${
    flipH ? -1 : 1
  }) scaleY(${flipV ? -1 : 1})`;

  const dimensions = source && geometry ? outputSize(source, geometry) : null;

  return (
    <div
      ref={overlayRef}
      className="viewer-overlay"
      role="dialog"
      aria-modal="true"
      aria-label="图片查看器"
      tabIndex={-1}
    >
      <header className="viewer-topbar">
        <div className="viewer-topbar-group">
          <IconButton label="关闭查看器" onClick={onClose}>
            <X size={16} />
          </IconButton>
          <IconButton label="上一张" disabled={index <= 0} onClick={() => step(-1)}>
            <ChevronLeft size={16} />
          </IconButton>
          <IconButton
            label="下一张"
            disabled={index < 0 || (index >= items.length - 1 && !session.hasMore())}
            onClick={() => step(1)}
          >
            <ChevronRight size={16} />
          </IconButton>
          <div className="viewer-title">
            <strong>#{activeId}</strong>
            <span>
              {index >= 0
                ? `${(index + 1).toLocaleString()} / ${items.length.toLocaleString()}${
                    session.hasMore() ? "+" : ""
                  }`
                : "不在当前列表"}
            </span>
            {source ? (
              <span>
                {source.format.toUpperCase()} · {formatDimensions(source.width, source.height)}
              </span>
            ) : null}
          </div>
        </div>

        <div className="viewer-topbar-group viewer-topbar-center">
          <IconButton label="缩小" onClick={() => zoomBy(0.8)}>
            <ZoomOut size={15} />
          </IconButton>
          <button type="button" className="viewer-zoom-label" onClick={fitView} title="适应窗口">
            {Math.round(zoom * 100)}%
          </button>
          <IconButton label="放大" onClick={() => zoomBy(1.25)}>
            <ZoomIn size={15} />
          </IconButton>
          <Button
            size="sm"
            variant={fitActive ? "primary" : "ghost"}
            onClick={fitView}
          >
            适应
          </Button>
          <Button
            size="sm"
            variant={!fitActive && zoom === 1 ? "primary" : "ghost"}
            onClick={actualSize}
          >
            1:1
          </Button>
          <IconButton label={fullscreen ? "退出全屏" : "全屏"} onClick={() => void toggleFullscreen()}>
            {fullscreen ? <Minimize size={15} /> : <Maximize size={15} />}
          </IconButton>
        </div>

        <div className="viewer-topbar-group">
          {geometryEdits ? <span className="viewer-dirty">未保存</span> : null}
          <IconButton label="复制图片" disabled={!source} onClick={() => void copyImage()}>
            <Copy size={15} />
          </IconButton>
          <IconButton
            label={
              poseBusy
                ? "正在识别姿势…"
                : hasPose
                  ? poseOpen
                    ? "隐藏姿势骨架"
                    : "显示姿势骨架"
                  : "生成姿势骨架"
            }
            className={poseOpen ? "is-active" : ""}
            disabled={poseBusy || (!hasPose && !onGeneratePose)}
            onClick={() => void togglePose()}
          >
            {poseBusy ? <Spinner size={14} /> : <PersonStanding size={15} />}
          </IconButton>
          <IconButton
            label={sidebarOpen ? "隐藏详情侧栏" : "显示详情侧栏"}
            className={sidebarOpen ? "is-active" : ""}
            onClick={onToggleSidebar}
          >
            <PanelRight size={15} />
          </IconButton>
        </div>
      </header>

      <div className="viewer-body">
        {source && geometry ? (
          <ViewerTools
            source={source}
            geometry={geometry}
            cropMode={cropMode}
            cropDraft={cropDraft}
            hasChanges={hasChanges}
            saving={saving}
            onGeometryChange={(updater) =>
              setGeometry((previous) => (previous ? updater(previous) : previous))
            }
            onRotate={rotateBy}
            onFlip={flip}
            onEnterCrop={enterCrop}
            onApplyCrop={applyCrop}
            onCancelCrop={cancelCrop}
            onClearCrop={clearCrop}
            onReset={resetGeometry}
            onSave={() => void save()}
          />
        ) : (
          <aside className="viewer-tools viewer-tools-loading">
            <Spinner size={18} />
            <span>正在准备查看器…</span>
          </aside>
        )}

        <div className="viewer-canvas-wrap">
          <div
            ref={canvasRef}
            className={`viewer-canvas${cropMode ? " is-cropping" : ""}${
              dragging ? " is-dragging" : ""
            }${cropDragging ? " is-crop-dragging" : ""}`}
            onPointerDown={handlePointerDown}
            onPointerMove={handlePointerMove}
            onPointerUp={endPointer}
            onPointerCancel={endPointer}
            onDoubleClick={() => (fitActive ? actualSize() : fitView())}
            onContextMenu={(event) => event.preventDefault()}
          >
            {sourceStatus === "loading" ? (
              <div className="viewer-state">
                <Spinner size={22} />
                <strong>正在读取原图</strong>
                <span>大图可能需要生成预览…</span>
              </div>
            ) : null}

            {sourceStatus === "error" ? (
              <div className="viewer-state viewer-state-error">
                <AlertTriangle size={24} />
                <strong>无法打开图片</strong>
                <span>{sourceError}</span>
                <Button
                  size="sm"
                  variant="primary"
                  icon={<RefreshCw size={13} />}
                  onClick={() => setRetry((value) => value + 1)}
                >
                  重试
                </Button>
              </div>
            ) : null}

            {source && sourceStatus === "ready" ? (
              <div
                className="viewer-pan"
                style={{
                  transform: `translate(-50%, -50%) translate(${pan.x}px, ${pan.y}px)`,
                }}
              >
                <div
                  className="viewer-clip"
                  style={{ width: clipWidth, height: clipHeight }}
                >
                  <div
                    ref={stageRef}
                    className="viewer-stage"
                    style={{
                      width: fullSize.width * zoom,
                      height: fullSize.height * zoom,
                      left: appliedCrop ? -appliedCrop.x * zoom : 0,
                      top: appliedCrop ? -appliedCrop.y * zoom : 0,
                    }}
                  >
                    <div
                      className="viewer-image-wrap"
                      style={{
                        width: source.displayWidth * zoom,
                        height: source.displayHeight * zoom,
                        transform: imageTransform,
                      }}
                    >
                      <img
                        src={withRetry(source.url, retry)}
                        alt=""
                        draggable={false}
                        style={{ imageOrientation: "from-image" }}
                        onLoad={() => setImageLoaded(true)}
                        onError={() => {
                          setImageLoaded(false);
                          setImageFailed(true);
                        }}
                      />
                      {poseOpen && pose && hasPose ? (
                        <PoseOverlay
                          pose={pose}
                          width={source.displayWidth}
                          height={source.displayHeight}
                        />
                      ) : null}
                    </div>

                    {overlayRect ? (
                      <div
                        className={`viewer-crop-rect${cropDraft ? " is-draft" : ""}`}
                        style={{
                          left: overlayRect.x * zoom,
                          top: overlayRect.y * zoom,
                          width: overlayRect.width * zoom,
                          height: overlayRect.height * zoom,
                        }}
                      >
                        <span className="viewer-crop-size">
                          {overlayOriginal
                            ? `${overlayOriginal.width.toLocaleString()} × ${overlayOriginal.height.toLocaleString()}`
                            : ""}
                        </span>
                      </div>
                    ) : null}
                  </div>
                </div>
              </div>
            ) : null}

            {source && !imageLoaded && sourceStatus === "ready" && !imageFailed ? (
              <div className="viewer-image-loading">
                <Spinner size={18} />
              </div>
            ) : null}

            {source && imageFailed ? (
              <div className="viewer-state viewer-state-error">
                <AlertTriangle size={24} />
                <strong>图片解码失败</strong>
                <span>请确认原图仍存在，或检查输出格式是否被 WebView2 支持。</span>
                <Button
                  size="sm"
                  variant="primary"
                  icon={<RefreshCw size={13} />}
                  onClick={() => {
                    setImageFailed(false);
                    setRetry((value) => value + 1);
                  }}
                >
                  重新加载
                </Button>
              </div>
            ) : null}
          </div>

          <div className="viewer-statusbar">
            {source ? (
              <>
                <span>{formatDimensions(source.width, source.height)}</span>
                <span>{formatBytes(source.fileSize)}</span>
                <span>{source.downsampled ? "降采样预览" : source.preview ? "转换预览" : "原图"}</span>
                <span className="mono">{source.contentHash.slice(0, 16)}…</span>
                {dimensions ? (
                  <span>
                    输出 {dimensions.width.toLocaleString()} × {dimensions.height.toLocaleString()}
                  </span>
                ) : null}
                {geometry?.crop ? <span>已裁剪</span> : null}
                {geometry?.rotate ? <span>旋转 {geometry.rotate * 90}°</span> : null}
              </>
            ) : (
              <span>等待图片…</span>
            )}
            <span className="viewer-status-spacer" />
            <span>{source?.deleted ? "回收站" : "Esc 关闭 · ←/→ 切换 · 0 适应 · 1 原尺寸"}</span>
          </div>
        </div>

        {sidebarOpen && sidebar ? <div className="viewer-sidebar">{sidebar}</div> : null}
      </div>
    </div>
  );
}

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import {
  ChevronLeft,
  ChevronRight,
  Copy,
  Film,
  Maximize,
  Minimize,
  PanelRight,
  PersonStanding,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide-react";
import { useNotices } from "../../shared/notices";
import { Button, IconButton } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { errorMessage } from "../../shared/errors";
import type { ViewerSession } from "../../shared/viewerSession";
import { viewerApi } from "./api";
import {
  buildEditRequest,
  clampCrop,
  fitZoomForSize,
  flipGeometry,
  formatBytes,
  formatDimensions,
  gifStaticPreview,
  hasPixelChanges,
  initialGeometry,
  outputSize,
  rectBetween,
  rotateGeometry,
  transformedSize,
  type GeometryState,
} from "./model";
import { Filmstrip } from "./Filmstrip";
import { hasVisiblePose, visiblePoseCount } from "./PoseOverlay";
import type { CropRect, PoseData, ViewerSource } from "./types";
import { useEditEstimate } from "./useEditEstimate";
import { useEditHistory } from "./useEditHistory";
import { useViewerSource } from "./useViewerSource";
import { ViewerStage } from "./ViewerStage";
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
  filmstripOpen: boolean;
  onToggleFilmstrip: () => void;
  fullscreen: boolean;
  onToggleFullscreen: () => void;
}

const POSE_OPACITY_KEY = "lilith.poseOpacity";

function initialPoseOpacity(): number {
  try {
    const value = Number(localStorage.getItem(POSE_OPACITY_KEY));
    return Number.isFinite(value) && value >= 0.2 && value <= 1 ? value : 0.9;
  } catch {
    return 0.9;
  }
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
  filmstripOpen,
  onToggleFilmstrip,
  fullscreen,
  onToggleFullscreen,
}: ImageViewerProps) {
  const { notify } = useNotices();
  const items = useSyncExternalStore(session.subscribe, session.getItems, session.getItems);
  const history = useEditHistory();
  const resetHistory = history.reset;

  const [activeId, setActiveId] = useState(session.initialId);
  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;

  const [pose, setPose] = useState<PoseData | null>(null);
  const [poseOpen, setPoseOpen] = useState(false);
  const [poseBusy, setPoseBusy] = useState(false);
  const [poseOpacity, setPoseOpacity] = useState(initialPoseOpacity);
  const [saving, setSaving] = useState(false);

  const [zoom, setZoom] = useState(1);
  const [fitActive, setFitActive] = useState(true);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const [viewport, setViewport] = useState({ width: 0, height: 0 });
  const [dragging, setDragging] = useState(false);
  const [slow, setSlow] = useState(false);

  const [cropMode, setCropMode] = useState(false);
  const [cropDraft, setCropDraft] = useState<CropRect | null>(null);
  const [cropDragging, setCropDragging] = useState(false);
  const [pendingStep, setPendingStep] = useState(0);
  const cropStart = useRef<{ x: number; y: number } | null>(null);
  const dragRef = useRef<{ x: number; y: number; panX: number; panY: number } | null>(null);

  const overlayRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLDivElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const fitActiveRef = useRef(fitActive);
  fitActiveRef.current = fitActive;
  const showStageRef = useRef(false);
  const zoomRef = useRef(zoom);
  zoomRef.current = zoom;
  const panRef = useRef(pan);
  panRef.current = pan;

  const index = items.findIndex((item) => item.id === activeId);
  const item = index >= 0 ? items[index] : undefined;
  /**
   * 原图内容的身份。空间优化与像素编辑都会换掉 `contentHash`，只按 id 判断会一直
   * 显示旧内容；旧文件被释放后还会变成坏图，编辑也基于过期源。
   */
  const contentKey = item ? `${item.contentHash}:${item.fileSize}` : "";

  const resetForImage = useCallback(() => {
    setPose(null);
    setPoseOpen(false);
    setCropMode(false);
    setCropDraft(null);
    setPendingStep(0);
    setDragging(false);
    setCropDragging(false);
    cropStart.current = null;
    dragRef.current = null;
    resetHistory(null);
  }, [resetHistory]);

  const handleSourceReady = useCallback(
    (next: ViewerSource) => {
      resetHistory(initialGeometry(next));
    },
    [resetHistory],
  );

  const {
    source,
    sourceError,
    activeLoad,
    activeMedia,
    patchMedia,
    retry,
    retryLoad,
    resetMedia,
    reload,
  } = useViewerSource({
    initialId: session.initialId,
    activeId,
    contentKey,
    items,
    fitActiveRef,
    setZoom,
    setPose,
    onSwitch: resetForImage,
    onReady: handleSourceReady,
  });

  const geometry = source ? history.geometry : null;

  const hasChanges = source && geometry ? hasPixelChanges(source, geometry) : false;
  const hasPose = hasVisiblePose(pose);
  const thumbItem = item != null && !activeMedia.thumbFailed ? item : null;
  const hasThumb = thumbItem != null;
  const showStage =
    (source != null || hasThumb) &&
    activeLoad.status !== "error" &&
    !activeMedia.failed;
  showStageRef.current = showStage;
  const cropping = source != null && cropMode;
  const poseVisible = source != null && poseOpen && pose != null && hasPose;
  const imageHasPose = source != null && hasPose;

  const applyHistoryGeometry = useCallback((next: GeometryState | null) => {
    if (!next) return;
    setCropMode(false);
    setCropDraft(next.crop);
  }, []);
  const undoHistory = useCallback(() => {
    applyHistoryGeometry(history.undo());
  }, [applyHistoryGeometry, history.undo]);
  const redoHistory = useCallback(() => {
    applyHistoryGeometry(history.redo());
  }, [applyHistoryGeometry, history.redo]);
  const { estimate, loading: estimating } = useEditEstimate(
    activeId,
    source,
    geometry,
    Boolean(source?.editable && !source.deleted && hasChanges && !cropMode),
  );

  useEffect(() => {
    session.onActiveChange(activeId);
    onActiveChange?.(activeId);
  }, [activeId, onActiveChange, session]);

  // 姿势骨架不透明度是查看器偏好，不进入图库设置文件。
  useEffect(() => {
    try {
      localStorage.setItem(POSE_OPACITY_KEY, String(poseOpacity));
    } catch {
      // 忽略无法持久化的浏览器限制。
    }
  }, [poseOpacity]);

  // 把焦点从网格移进查看器：网格的键盘处理挂在容器上，焦点留在网格会让
  // ←/→、Esc 同时作用于网格和查看器。
  useEffect(() => {
    overlayRef.current?.focus();
  }, []);

  // 视口尺寸用于“适应窗口”。首次测量放在 layout effect，避免占位层先按 100% 画一帧。
  useLayoutEffect(() => {
    const element = canvasRef.current;
    if (!element) return;
    const width = element.clientWidth;
    const height = element.clientHeight;
    if (width > 0 && height > 0) setViewport({ width, height });
  }, []);

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

  const displayWidth = source?.displayWidth ?? item?.width ?? 0;
  const displayHeight = source?.displayHeight ?? item?.height ?? 0;
  const rotate = source && geometry ? geometry.rotate : 0;
  const fit = fitZoomForSize(
    displayWidth,
    displayHeight,
    rotate,
    viewport.width,
    viewport.height,
  );

  useLayoutEffect(() => {
    setFitActive(true);
    setPan({ x: 0, y: 0 });
  }, [activeId]);

  useLayoutEffect(() => {
    if (fitActive) {
      setZoom(fit);
      setPan({ x: 0, y: 0 });
    }
  }, [fitActive, fit]);

  const waitingForSource = activeLoad.status === "loading" && hasThumb;
  const waitingForHighRes =
    source != null && !activeMedia.loaded && !activeMedia.failed;
  const delayable = waitingForSource || (waitingForHighRes && hasThumb);

  useEffect(() => {
    setSlow(false);
    if (!delayable) return;
    const timer = window.setTimeout(() => setSlow(true), 150);
    return () => window.clearTimeout(timer);
  }, [delayable, activeId, retry]);

  // 有缩略图时先展示占位；没有占位才立即显示小 spinner。
  const showSmallSpinner =
    waitingForHighRes && !hasThumb ? true : delayable && slow;

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
      if (!showStageRef.current) return;
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

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (document.querySelector(".modal-backdrop")) return;
      // 滑块聚焦时快捷键仍然生效；文本框与下拉保留浏览器原生行为。
      const target = event.target as HTMLElement | null;
      const editingText =
        target != null &&
        !(target instanceof HTMLInputElement && target.type === "range") &&
        ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName);
      const modified = event.ctrlKey || event.metaKey;
      const key = event.key.toLowerCase();
      if (!editingText && modified && (key === "z" || key === "y")) {
        event.preventDefault();
        if (key === "y" || event.shiftKey) redoHistory();
        else undoHistory();
        return;
      }
      if (editingText) return;

      switch (event.key) {
        case "Escape":
          event.preventDefault();
          if (cropping) {
            setCropMode(false);
            setCropDraft(geometry?.crop ?? null);
          } else if (poseVisible) {
            setPoseOpen(false);
          } else if (fullscreen) {
            onToggleFullscreen();
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
          onToggleFullscreen();
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
    actualSize,
    cropping,
    fitView,
    fullscreen,
    geometry?.crop,
    onClose,
    onToggleFullscreen,
    poseVisible,
    redoHistory,
    step,
    undoHistory,
    zoomBy,
  ]);

  const rotateBy = (delta: number) => {
    if (!source || !geometry) return;
    history.commit(rotateGeometry(geometry, source, delta), {
      label: delta > 0 ? "旋转 90°" : "旋转 -90°",
    });
    setCropDraft(null);
    setCropMode(false);
    setFitActive(true);
    setPan({ x: 0, y: 0 });
  };

  const flip = (axis: "horizontal" | "vertical") => {
    if (!source || !geometry) return;
    const next = flipGeometry(geometry, source, axis);
    history.commit(next, { label: axis === "horizontal" ? "水平翻转" : "垂直翻转" });
    setCropDraft(next.crop);
    setCropMode(false);
  };

  const commitGeometry = (
    updater: (previous: GeometryState) => GeometryState,
    label: string,
    mergeKey?: string,
  ) => {
    if (!geometry) return;
    history.commit(updater(geometry), { label, mergeKey });
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
    history.commit(
      { ...geometry, crop, resize: null },
      { label: geometry.crop ? "调整裁剪" : "裁剪" },
    );
    setCropDraft(null);
    setCropMode(false);
  };

  const clearCrop = () => {
    if (!geometry) return;
    history.commit({ ...geometry, crop: null, resize: null }, { label: "清除裁剪" });
    setCropDraft(null);
    setCropMode(false);
  };

  const resetGeometry = () => {
    if (!source || !geometry) return;
    history.commit(initialGeometry(source), { label: "重置" });
    setCropDraft(null);
    setCropMode(false);
    setFitActive(true);
    setPan({ x: 0, y: 0 });
  };

  const save = async () => {
    if (!source || !geometry || saving || !hasChanges || cropMode) return;
    setSaving(true);
    try {
      const result = await viewerApi.saveImageEdit(
        buildEditRequest(activeId, source, geometry),
      );
      if (!result.changed) {
        notify("内容没有变化", "info");
      } else {
        notify(
          "编辑已保存",
          "success",
          `新内容 ${formatDimensions(result.width, result.height)} · ${formatBytes(result.fileSize)}`,
        );
        session.onSaved(activeId, {
          contentHash: result.contentHash,
          fileExt: result.fileExt,
          fileSize: result.fileSize,
          width: result.width,
          height: result.height,
          format: result.format,
          hasAlpha: result.hasAlpha,
          thumbVersion: result.thumbVersion,
        });
      }
      // 重新载入查看器源图、回到适应窗口并清空撤销栈；列表本身已就地更新，不重置查询与滚动位置。
      resetMedia(activeId);
      setFitActive(true);
      reload();
      onSaved?.(activeId);
    } catch (cause) {
      notify("保存失败", "error", errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };

  const togglePose = async () => {
    if (imageHasPose) {
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
    if (!showStage) return;
    if (cropping && event.button === 0) {
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

  const handleThumbError = () => {
    if (activeIdRef.current !== activeId) return;
    patchMedia(activeId, { thumbFailed: true });
  };

  const handleImageLoad = () => {
    if (activeIdRef.current !== activeId) return;
    patchMedia(activeId, { loaded: true, failed: false });
  };

  const handleImageError = () => {
    if (activeIdRef.current !== activeId) return;
    patchMedia(activeId, { loaded: false, failed: true });
  };

  const flipH = geometry?.flipHorizontal ?? false;
  const flipV = geometry?.flipVertical ?? false;
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
      {!fullscreen ? (
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
            <IconButton
              label={fullscreen ? "退出全屏" : "全屏"}
              onClick={onToggleFullscreen}
            >
              {fullscreen ? <Minimize size={15} /> : <Maximize size={15} />}
            </IconButton>
          </div>

          <div className="viewer-topbar-group">
            {hasChanges ? <span className="viewer-dirty">未保存</span> : null}
            <IconButton label="复制图片" disabled={!source} onClick={() => void copyImage()}>
              <Copy size={15} />
            </IconButton>
            <IconButton
              label={
                poseBusy
                  ? "正在识别姿势…"
                  : imageHasPose
                    ? poseOpen
                      ? "隐藏姿势骨架"
                      : "显示姿势骨架"
                    : "生成姿势骨架"
              }
              className={poseVisible ? "is-active" : ""}
              disabled={poseBusy || (!imageHasPose && !onGeneratePose)}
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
      ) : null}

      <div className="viewer-body">
        {!fullscreen && source && geometry ? (
          <ViewerTools
            source={source}
            inTemp={session.inTemp}
            geometry={geometry}
            cropMode={cropMode}
            cropDraft={cropDraft}
            hasChanges={hasChanges}
            saving={saving}
            estimate={estimate}
            estimating={estimating}
            canUndo={history.canUndo}
            canRedo={history.canRedo}
            undoLabel={history.undoLabel}
            redoLabel={history.redoLabel}
            onCommit={commitGeometry}
            onUndo={undoHistory}
            onRedo={redoHistory}
            onRotate={rotateBy}
            onFlip={flip}
            onEnterCrop={enterCrop}
            onApplyCrop={applyCrop}
            onCancelCrop={cancelCrop}
            onClearCrop={clearCrop}
            onReset={resetGeometry}
            onSave={() => void save()}
          />
        ) : !fullscreen ? (
          <aside className="viewer-tools viewer-tools-loading">
            <Spinner size={18} />
            <span>正在准备查看器…</span>
          </aside>
        ) : null}

        <div className="viewer-canvas-wrap">
          <ViewerStage
            canvasRef={canvasRef}
            stageRef={stageRef}
            showLoading={activeLoad.status === "loading" && !hasThumb}
            sourceError={sourceError}
            showStage={showStage}
            showSmallSpinner={showSmallSpinner}
            cropping={cropping}
            dragging={dragging}
            cropDragging={cropDragging}
            zoom={zoom}
            pan={pan}
            source={source}
            displayWidth={displayWidth}
            displayHeight={displayHeight}
            rotate={rotate}
            flipH={flipH}
            flipV={flipV}
            geometryCrop={geometry?.crop ?? null}
            cropDraft={cropDraft}
            thumbItem={thumbItem}
            mediaLoaded={activeMedia.loaded}
            mediaFailed={activeMedia.failed}
            pose={pose}
            poseVisible={poseVisible}
            poseOpacity={poseOpacity}
            retry={retry}
            onPointerDown={handlePointerDown}
            onPointerMove={handlePointerMove}
            onPointerUp={endPointer}
            onPointerCancel={endPointer}
            onDoubleClick={() => (fitActive ? actualSize() : fitView())}
            onThumbError={handleThumbError}
            onImageLoad={handleImageLoad}
            onImageError={handleImageError}
            onRetryLoad={retryLoad}
          />

          {!fullscreen && poseVisible && pose ? (
            <div className="viewer-pose-panel">
              <span>姿势 {visiblePoseCount(pose)}/33</span>
              <label>
                不透明度
                <input
                  type="range"
                  min={20}
                  max={100}
                  value={Math.round(poseOpacity * 100)}
                  onChange={(event) => setPoseOpacity(Number(event.target.value) / 100)}
                />
              </label>
            </div>
          ) : null}

          {!fullscreen && filmstripOpen && items.length > 0 ? (
            <Filmstrip
              items={items}
              activeId={activeId}
              hasMore={session.hasMore()}
              onSelect={(imageId) => setActiveId(imageId)}
              onLoadMore={session.loadMore}
            />
          ) : null}

          {!fullscreen ? (
            <div className="viewer-statusbar">
              <button
                type="button"
                className={`viewer-filmstrip-toggle${filmstripOpen ? " is-active" : ""}`}
                title={filmstripOpen ? "隐藏附近图片" : "显示附近图片"}
                onClick={onToggleFilmstrip}
              >
                <Film size={12} />
                胶片带
              </button>
              {source ? (
                <>
                  <span>{formatDimensions(source.width, source.height)}</span>
                  <span>{formatBytes(source.fileSize)}</span>
                  <span>
                    {gifStaticPreview(source)
                      ? "静态预览 · 动画不可用"
                      : source.downsampled
                        ? "降采样预览"
                        : source.preview
                          ? "转换预览"
                          : "原图"}
                  </span>
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
              <span>
                {source?.deleted ? "回收站" : "Esc 关闭 · ←/→ 切换 · 0 适应 · 1 原尺寸 · F11 全屏"}
              </span>
            </div>
          ) : null}
        </div>

        {!fullscreen && sidebarOpen && sidebar ? (
          <div className="viewer-sidebar">{sidebar}</div>
        ) : null}
      </div>
    </div>
  );
}

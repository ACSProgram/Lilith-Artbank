import type { PointerEvent as ReactPointerEvent, RefObject } from "react";
import { AlertTriangle, RefreshCw } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { thumbnailUrl } from "../../shared/thumbUrl";
import type { ViewerSessionItem } from "../../shared/viewerSession";
import { withRetry } from "./api";
import { cropToOriginal, transformedSize } from "./model";
import { PoseOverlay } from "./PoseOverlay";
import type { CropRect, PoseData, ViewerSource } from "./types";

interface ViewerStageProps {
  canvasRef: RefObject<HTMLDivElement | null>;
  stageRef: RefObject<HTMLDivElement | null>;
  showLoading: boolean;
  sourceError: string | null;
  showStage: boolean;
  showSmallSpinner: boolean;
  cropping: boolean;
  dragging: boolean;
  cropDragging: boolean;
  zoom: number;
  pan: { x: number; y: number };
  source: ViewerSource | null;
  displayWidth: number;
  displayHeight: number;
  rotate: number;
  flipH: boolean;
  flipV: boolean;
  geometryCrop: CropRect | null;
  cropDraft: CropRect | null;
  thumbItem: ViewerSessionItem | null;
  mediaLoaded: boolean;
  mediaFailed: boolean;
  pose: PoseData | null;
  poseVisible: boolean;
  poseOpacity: number;
  retry: number;
  onPointerDown: (event: ReactPointerEvent<HTMLDivElement>) => void;
  onPointerMove: (event: ReactPointerEvent<HTMLDivElement>) => void;
  onPointerUp: (event: ReactPointerEvent<HTMLDivElement>) => void;
  onPointerCancel: (event: ReactPointerEvent<HTMLDivElement>) => void;
  onDoubleClick: () => void;
  onThumbError: () => void;
  onImageLoad: () => void;
  onImageError: () => void;
  onRetryLoad: () => void;
}

/** 查看器画布：占位态、错误态、缩略图渐进层、原图、姿势骨架与裁剪框。 */
export function ViewerStage({
  canvasRef,
  stageRef,
  showLoading,
  sourceError,
  showStage,
  showSmallSpinner,
  cropping,
  dragging,
  cropDragging,
  zoom,
  pan,
  source,
  displayWidth,
  displayHeight,
  rotate,
  flipH,
  flipV,
  geometryCrop,
  cropDraft,
  thumbItem,
  mediaLoaded,
  mediaFailed,
  pose,
  poseVisible,
  poseOpacity,
  retry,
  onPointerDown,
  onPointerMove,
  onPointerUp,
  onPointerCancel,
  onDoubleClick,
  onThumbError,
  onImageLoad,
  onImageError,
  onRetryLoad,
}: ViewerStageProps) {
  const fullSize = transformedSize(displayWidth, displayHeight, rotate);
  const appliedCrop = !cropping ? (geometryCrop ?? null) : null;
  const clipWidth = (appliedCrop ? appliedCrop.width : fullSize.width) * zoom;
  const clipHeight = (appliedCrop ? appliedCrop.height : fullSize.height) * zoom;
  const overlayRect = cropping ? (cropDraft ?? geometryCrop ?? null) : null;
  const overlayOriginal =
    overlayRect && source ? cropToOriginal(overlayRect, rotate, source) : null;
  const imageTransform = `translate(-50%, -50%) rotate(${rotate * 90}deg) scaleX(${
    flipH ? -1 : 1
  }) scaleY(${flipV ? -1 : 1})`;

  return (
    <div
      ref={canvasRef}
      className={`viewer-canvas${cropping ? " is-cropping" : ""}${
        dragging ? " is-dragging" : ""
      }${cropDragging ? " is-crop-dragging" : ""}`}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      onDoubleClick={onDoubleClick}
      onContextMenu={(event) => event.preventDefault()}
    >
      {showLoading ? (
        <div className="viewer-state">
          <Spinner size={22} />
          <strong>正在读取原图</strong>
          <span>大图可能需要生成预览…</span>
        </div>
      ) : null}

      {sourceError != null ? (
        <div className="viewer-state viewer-state-error">
          <AlertTriangle size={24} />
          <strong>无法打开图片</strong>
          <span>{sourceError}</span>
          <Button
            size="sm"
            variant="primary"
            icon={<RefreshCw size={13} />}
            onClick={onRetryLoad}
          >
            重试
          </Button>
        </div>
      ) : null}

      {showStage ? (
        <div
          className="viewer-pan"
          style={{
            transform: `translate(-50%, -50%) translate(${pan.x}px, ${pan.y}px)`,
          }}
        >
          <div className="viewer-clip" style={{ width: clipWidth, height: clipHeight }}>
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
                className={`viewer-image-wrap${source?.hasAlpha ? " is-alpha" : ""}`}
                style={{
                  width: displayWidth * zoom,
                  height: displayHeight * zoom,
                  transform: imageTransform,
                }}
              >
                {thumbItem ? (
                  <img
                    className={`viewer-thumb-layer${mediaLoaded ? " is-retired" : ""}`}
                    src={thumbnailUrl(thumbItem.id, thumbItem.thumbVersion)}
                    alt=""
                    draggable={false}
                    onError={onThumbError}
                  />
                ) : null}

                {source ? (
                  <img
                    className={`viewer-image${mediaLoaded ? " is-loaded" : ""}`}
                    src={withRetry(source.url, retry)}
                    alt=""
                    draggable={false}
                    style={{ imageOrientation: "from-image" }}
                    onLoad={onImageLoad}
                    onError={onImageError}
                  />
                ) : null}

                {poseVisible && pose ? (
                  <PoseOverlay
                    pose={pose}
                    width={source?.displayWidth ?? displayWidth}
                    height={source?.displayHeight ?? displayHeight}
                    zoom={zoom}
                    opacity={poseOpacity}
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

      {showSmallSpinner ? (
        <div className="viewer-image-loading">
          <Spinner size={18} />
        </div>
      ) : null}

      {source && mediaFailed ? (
        <div className="viewer-state viewer-state-error">
          <AlertTriangle size={24} />
          <strong>这张图片打不开</strong>
          <span>原图可能已被移动或删除，也可能是当前不支持的格式。</span>
          <Button
            size="sm"
            variant="primary"
            icon={<RefreshCw size={13} />}
            onClick={onRetryLoad}
          >
            重新加载
          </Button>
        </div>
      ) : null}
    </div>
  );
}

import type {
  CropRect,
  OutputFormat,
  ResizeSpec,
  SaveEditRequest,
  ViewerSource,
} from "./types";

export const DEFAULT_QUALITY = 90;
export const MAX_EDIT_PIXELS = 50_000_000;
export const MAX_EDIT_DIMENSION = 65_535;

export interface GeometryState {
  /** 顺时针 90° 的次数，0-3。 */
  rotate: number;
  flipHorizontal: boolean;
  flipVertical: boolean;
  /** 显示坐标（旋转/翻转后、缩放前）。 */
  crop: CropRect | null;
  /** 原图像素尺寸；与裁剪后的原始尺寸相同时按未设置处理。 */
  resize: ResizeSpec | null;
  outputFormat: OutputFormat;
  quality: number;
}

export function transformedSize(width: number, height: number, rotate: number) {
  return rotate % 2 === 0 ? { width, height } : { width: height, height: width };
}

export function defaultOutputFormat(format: string, hasAlpha: boolean): OutputFormat {
  if (format === "jpeg" || format === "jpg") return "jpeg";
  if (format === "png") return "png";
  if (format === "webp") return "webp";
  return hasAlpha ? "png" : "jpeg";
}

export function initialGeometry(source: ViewerSource): GeometryState {
  return {
    rotate: 0,
    flipHorizontal: false,
    flipVertical: false,
    crop: null,
    resize: null,
    outputFormat: defaultOutputFormat(source.format, source.hasAlpha),
    quality: DEFAULT_QUALITY,
  };
}

export function sameGeometry(left: GeometryState, right: GeometryState): boolean {
  return (
    left.rotate === right.rotate &&
    left.flipHorizontal === right.flipHorizontal &&
    left.flipVertical === right.flipVertical &&
    left.outputFormat === right.outputFormat &&
    left.quality === right.quality &&
    sameRect(left.crop, right.crop) &&
    sameRect(left.resize, right.resize)
  );
}

function sameRect(
  left: CropRect | ResizeSpec | null,
  right: CropRect | ResizeSpec | null,
): boolean {
  if (!left || !right) return left === right;
  if (left.width !== right.width || left.height !== right.height) return false;
  // 只有裁剪矩形带坐标；尺寸 spec 落到这里说明两者是同一种形状。
  if (!("x" in left) && !("x" in right)) return true;
  return "x" in left && "x" in right && left.x === right.x && left.y === right.y;
}

/** 顺时针旋转后裁剪矩形的映射：(x, y, w, h) → (H - y - h, x, h, w)。 */
function rotateCropOnce(crop: CropRect, bounds: { width: number; height: number }): CropRect {
  return {
    x: bounds.height - crop.y - crop.height,
    y: crop.x,
    width: crop.height,
    height: crop.width,
  };
}

/** 旋转整图时把裁剪矩形一起转过去，而不是清空。 */
export function rotateGeometry(
  state: GeometryState,
  source: ViewerSource,
  delta: number,
): GeometryState {
  const steps = ((delta % 4) + 4) % 4;
  if (steps === 0) return state;
  let rotate = state.rotate;
  let crop = state.crop;
  for (let index = 0; index < steps; index += 1) {
    if (crop) {
      const bounds = transformedSize(source.displayWidth, source.displayHeight, rotate);
      crop = rotateCropOnce(crop, bounds);
    }
    rotate = (rotate + 1) % 4;
  }
  const bounds = transformedSize(source.displayWidth, source.displayHeight, rotate);
  const oldBase = baseOutputSize(source, state);
  const nextBase = baseOutputSize(source, { ...state, rotate, crop });
  return {
    ...state,
    rotate,
    crop: crop ? clampCrop(crop, bounds.width, bounds.height) : null,
    resize: resizeForNewBase(state.resize, oldBase, nextBase),
  };
}

/**
 * 翻转按“显示空间”语义：旋转奇数次的图片，水平翻转对应原图的垂直翻转。
 * 裁剪矩形在同一坐标系里一起镜像。
 */
export function flipGeometry(
  state: GeometryState,
  source: ViewerSource,
  axis: "horizontal" | "vertical",
): GeometryState {
  const bounds = transformedSize(source.displayWidth, source.displayHeight, state.rotate);
  const horizontal = axis === "horizontal";
  const crop = state.crop
    ? horizontal
      ? { ...state.crop, x: bounds.width - state.crop.x - state.crop.width }
      : { ...state.crop, y: bounds.height - state.crop.y - state.crop.height }
    : null;
  const togglesHorizontalSource = horizontal
    ? state.rotate % 2 === 0
    : state.rotate % 2 === 1;
  return {
    ...state,
    flipHorizontal: togglesHorizontalSource
      ? !state.flipHorizontal
      : state.flipHorizontal,
    flipVertical: togglesHorizontalSource
      ? state.flipVertical
      : !state.flipVertical,
    crop: crop ? clampCrop(crop, bounds.width, bounds.height) : null,
    resize: state.resize,
  };
}

/** 旋转/裁剪改变基底后，按原来的相对比例重新计算输出尺寸并夹到合法范围。 */
export function resizeForNewBase(
  resize: ResizeSpec | null,
  oldBase: { width: number; height: number },
  newBase: { width: number; height: number },
): ResizeSpec | null {
  if (!resize) return null;
  if (oldBase.width === newBase.width && oldBase.height === newBase.height) {
    return resize;
  }
  if (resize.width === oldBase.width && resize.height === oldBase.height) {
    return null;
  }
  return clampResize({
    width: (resize.width / oldBase.width) * newBase.width,
    height: (resize.height / oldBase.height) * newBase.height,
  });
}

export function clampResize(resize: ResizeSpec): ResizeSpec {
  let width = clamp(Math.round(resize.width), 1, MAX_EDIT_DIMENSION);
  let height = clamp(Math.round(resize.height), 1, MAX_EDIT_DIMENSION);
  const pixels = width * height;
  if (pixels > MAX_EDIT_PIXELS) {
    const scale = Math.sqrt(MAX_EDIT_PIXELS / pixels);
    width = Math.max(1, Math.floor(width * scale));
    height = Math.max(1, Math.floor(height * scale));
  }
  return { width, height };
}

/** 发送给 Rust 的编辑参数；保存与体积预估共用同一套换算。 */
export function buildEditRequest(
  imageId: number,
  source: ViewerSource,
  geometry: GeometryState,
): SaveEditRequest {
  const base = baseOutputSize(source, geometry);
  return {
    imageId,
    rotate: geometry.rotate,
    flipHorizontal: geometry.flipHorizontal,
    flipVertical: geometry.flipVertical,
    crop: geometry.crop ? cropToOriginal(geometry.crop, geometry.rotate, source) : null,
    resize: normalizeResize(geometry.resize, base),
    outputFormat: geometry.outputFormat,
    quality: geometry.quality,
  };
}

/** 显示坐标下的裁剪 → 原图像素坐标（裁剪系与旋转后的坐标系一致）。 */
export function cropToOriginal(
  crop: CropRect,
  rotate: number,
  source: ViewerSource,
): CropRect {
  const even = rotate % 2 === 0;
  const scaleX = even
    ? source.width / source.displayWidth
    : source.height / source.displayHeight;
  const scaleY = even
    ? source.height / source.displayHeight
    : source.width / source.displayWidth;
  const bounds = transformedSize(source.width, source.height, rotate);

  const x = clamp(Math.round(crop.x * scaleX), 0, bounds.width - 1);
  const y = clamp(Math.round(crop.y * scaleY), 0, bounds.height - 1);
  const width = clamp(Math.round(crop.width * scaleX), 1, bounds.width - x);
  const height = clamp(Math.round(crop.height * scaleY), 1, bounds.height - y);
  return { x, y, width, height };
}

export function baseOutputSize(source: ViewerSource, geometry: GeometryState) {
  if (geometry.crop) {
    const crop = cropToOriginal(geometry.crop, geometry.rotate, source);
    return { width: crop.width, height: crop.height };
  }
  return transformedSize(source.width, source.height, geometry.rotate);
}

function resizeIsEffective(resize: ResizeSpec | null, base: { width: number; height: number }) {
  return resize != null && (resize.width !== base.width || resize.height !== base.height);
}

export function outputSize(source: ViewerSource, geometry: GeometryState) {
  const base = baseOutputSize(source, geometry);
  return resizeIsEffective(geometry.resize, base) ? geometry.resize! : base;
}

function sourceOutputFormat(format: string): OutputFormat | null {
  if (format === "jpeg" || format === "jpg") return "jpeg";
  if (format === "png") return "png";
  if (format === "webp") return "webp";
  return null;
}

/** 仅几何/尺寸修改，不含输出格式；用于“未保存”提示。 */
export function hasGeometryEdits(source: ViewerSource, geometry: GeometryState): boolean {
  const base = baseOutputSize(source, geometry);
  return (
    geometry.rotate !== 0 ||
    geometry.flipHorizontal ||
    geometry.flipVertical ||
    geometry.crop != null ||
    resizeIsEffective(geometry.resize, base)
  );
}

/** 有损格式调到非默认质量也算一次修改，与后端 `has_changes` 规则一致。 */
export function hasLossyQualityChange(geometry: GeometryState): boolean {
  return geometry.outputFormat !== "png" && geometry.quality !== DEFAULT_QUALITY;
}

export function hasPixelChanges(source: ViewerSource, geometry: GeometryState): boolean {
  return (
    hasGeometryEdits(source, geometry) ||
    geometry.outputFormat !== sourceOutputFormat(source.format) ||
    hasLossyQualityChange(geometry)
  );
}

export function normalizeResize(
  resize: ResizeSpec | null,
  base: { width: number; height: number },
): ResizeSpec | null {
  if (!resize) return null;
  return resize.width === base.width && resize.height === base.height ? null : resize;
}

export function fitZoomForSize(
  width: number,
  height: number,
  rotate: number,
  viewportWidth: number,
  viewportHeight: number,
): number {
  const size = transformedSize(width, height, rotate);
  if (size.width <= 0 || size.height <= 0 || viewportWidth <= 0 || viewportHeight <= 0) {
    return 1;
  }
  const padding = 72;
  return clamp(
    Math.min(
      (viewportWidth - padding) / size.width,
      (viewportHeight - padding) / size.height,
    ),
    0.02,
    1,
  );
}

/** 占位阶段直接传会话项的宽高，不依赖 `ViewerSource`。 */
export function fitZoom(
  source: ViewerSource,
  rotate: number,
  viewportWidth: number,
  viewportHeight: number,
): number {
  return fitZoomForSize(
    source.displayWidth,
    source.displayHeight,
    rotate,
    viewportWidth,
    viewportHeight,
  );
}

/** GIF 走 Rust 静态预览时，动画不可用。 */
export function gifStaticPreview(
  source: Pick<ViewerSource, "format" | "preview"> | null | undefined,
): boolean {
  return source?.format === "gif" && source.preview === true;
}

/** 占位按原图尺寸、高清按降采样尺寸时，用手动缩放保持屏幕上的实际大小。 */
export function rescaleZoom(
  zoom: number,
  previousDisplayWidth: number,
  nextDisplayWidth: number,
): number {
  if (previousDisplayWidth <= 0 || nextDisplayWidth <= 0) return zoom;
  return clamp(zoom * (previousDisplayWidth / nextDisplayWidth), 0.02, 8);
}

export function rectBetween(
  start: { x: number; y: number },
  end: { x: number; y: number },
  boundsWidth: number,
  boundsHeight: number,
): CropRect | null {
  const x = clamp(Math.min(start.x, end.x), 0, boundsWidth);
  const y = clamp(Math.min(start.y, end.y), 0, boundsHeight);
  const right = clamp(Math.max(start.x, end.x), 0, boundsWidth);
  const bottom = clamp(Math.max(start.y, end.y), 0, boundsHeight);
  const width = right - x;
  const height = bottom - y;
  if (width < 2 || height < 2) return null;
  return { x, y, width, height };
}

export function clampCrop(rect: CropRect, width: number, height: number): CropRect {
  const x = clamp(Math.round(rect.x), 0, Math.max(0, width - 1));
  const y = clamp(Math.round(rect.y), 0, Math.max(0, height - 1));
  return {
    x,
    y,
    width: clamp(Math.round(rect.width), 1, width - x),
    height: clamp(Math.round(rect.height), 1, height - y),
  };
}

export function resizeFromWidth(
  width: number,
  base: { width: number; height: number },
  locked: boolean,
): ResizeSpec {
  const safeWidth = Math.max(1, Math.round(width));
  if (!locked || base.width <= 0) {
    return { width: safeWidth, height: base.height };
  }
  return {
    width: safeWidth,
    height: Math.max(1, Math.round((safeWidth / base.width) * base.height)),
  };
}

export function resizeFromHeight(
  height: number,
  base: { width: number; height: number },
  locked: boolean,
): ResizeSpec {
  const safeHeight = Math.max(1, Math.round(height));
  if (!locked || base.height <= 0) {
    return { width: base.width, height: safeHeight };
  }
  return {
    width: Math.max(1, Math.round((safeHeight / base.height) * base.width)),
    height: safeHeight,
  };
}

export function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

// 体积格式化与图库共用一份实现；`node --test` 直接跑 .ts 时需要显式扩展名。
export { formatBytes } from "../../shared/format.ts";

export function formatDimensions(width: number, height: number): string {
  return `${width.toLocaleString()} × ${height.toLocaleString()}`;
}

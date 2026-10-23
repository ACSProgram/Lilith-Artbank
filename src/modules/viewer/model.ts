import type { CropRect, OutputFormat, ResizeSpec, ViewerSource } from "./types";

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
    quality: 90,
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

export function hasPixelChanges(source: ViewerSource, geometry: GeometryState): boolean {
  return (
    hasGeometryEdits(source, geometry) ||
    geometry.outputFormat !== sourceOutputFormat(source.format)
  );
}

export function normalizeResize(
  resize: ResizeSpec | null,
  base: { width: number; height: number },
): ResizeSpec | null {
  if (!resize) return null;
  return resize.width === base.width && resize.height === base.height ? null : resize;
}

export function fitZoom(
  source: ViewerSource,
  rotate: number,
  viewportWidth: number,
  viewportHeight: number,
): number {
  const size = transformedSize(source.displayWidth, source.displayHeight, rotate);
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

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

export function formatDimensions(width: number, height: number): string {
  return `${width.toLocaleString()} × ${height.toLocaleString()}`;
}

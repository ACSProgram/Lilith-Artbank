export type OutputFormat = "jpeg" | "png" | "webp";

export interface CropRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface ResizeSpec {
  width: number;
  height: number;
}

/** 查看器与批量编辑共用的原图几何信息。 */
export interface SourceGeometry {
  width: number;
  height: number;
  displayWidth: number;
  displayHeight: number;
}

export interface EditableSource extends SourceGeometry {
  format: string;
  hasAlpha: boolean;
}

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

export interface SaveEditRequest {
  imageId: number;
  rotate: number;
  flipHorizontal: boolean;
  flipVertical: boolean;
  crop: CropRect | null;
  resize: ResizeSpec | null;
  outputFormat: OutputFormat;
  quality: number;
}

export type BatchEditMode = "keepSize" | "canvas";
export type BatchFit = "cover" | "contain";
export type BatchOutputFormat = "keep" | OutputFormat;

export interface BatchCanvas {
  width: number;
  height: number;
  fit: BatchFit;
}

/** 一次批量编辑的声明式配方；后端按每张图的原始尺寸换算。 */
export interface BatchEditOptions {
  rotate: number;
  flipHorizontal: boolean;
  flipVertical: boolean;
  mode: BatchEditMode;
  canvas: BatchCanvas;
  /** canvas 模式是画布坐标；keepSize 模式不裁剪，必须为 null。 */
  crop: CropRect | null;
  outputFormat: BatchOutputFormat;
  /** null 表示按每张图的输出格式取默认值。 */
  quality: number | null;
  reversible: boolean;
}

export type OutputFormat = "jpeg" | "png" | "webp";

export interface ViewerSource {
  imageId: number;
  contentHash: string;
  fileExt: string;
  format: string;
  fileSize: number;
  thumbVersion: number;
  hasAlpha: boolean;
  width: number;
  height: number;
  displayWidth: number;
  displayHeight: number;
  preview: boolean;
  downsampled: boolean;
  editable: boolean;
  deleted: boolean;
  maxEditPixels: number;
  /** 由前端 api 层拼好；浏览器预览为 data URL。 */
  url: string;
}

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

export interface SaveEditResult {
  imageId: number;
  contentHash: string;
  fileExt: string;
  fileSize: number;
  width: number;
  height: number;
  format: string;
  hasAlpha: boolean;
  thumbVersion: number;
  changed: boolean;
  oldFileRemoved: boolean;
}

export interface PoseLandmark {
  x: number;
  y: number;
  z: number;
  visibility: number;
}

export interface PoseData {
  model: string;
  landmarks: PoseLandmark[];
}

/** 浏览器预览使用的行信息；Tauri 下不使用。 */
export interface ViewerFallback {
  width: number;
  height: number;
  contentHash: string;
  fileExt: string;
  format: string;
  fileSize: number;
  hasAlpha: boolean;
  deletedAt: number | null;
  thumbVersion: number;
}

/** 图库内部拖放使用的自定义 MIME；只在浏览器内传递 id，不触发 Tauri 原生拖放。 */
export const IMAGE_DRAG_MIME = "application/x-artbank-images";
export const FOLDER_DRAG_MIME = "application/x-artbank-folder";

export interface ImageDragPayload {
  imageIds: number[];
}

export function readImageDrag(dataTransfer: DataTransfer): number[] {
  try {
    const raw = dataTransfer.getData(IMAGE_DRAG_MIME);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as ImageDragPayload;
    return Array.isArray(parsed.imageIds)
      ? parsed.imageIds.filter((id) => Number.isInteger(id) && id > 0)
      : [];
  } catch {
    return [];
  }
}

export function readFolderDrag(dataTransfer: DataTransfer): number | null {
  const raw = dataTransfer.getData(FOLDER_DRAG_MIME);
  const id = Number(raw);
  return Number.isInteger(id) && id > 0 ? id : null;
}

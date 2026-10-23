const thumbOrigin = /Windows/i.test(navigator.userAgent)
  ? "http://thumb.localhost"
  : "thumb://localhost";

/** 缩略图编码版本；与后端写入的 `thumb_version` 一致，参与 URL 缓存失效。 */
export const THUMB_VERSION = 1;

/** 图库缩略图 URL；`version` 变化时浏览器缓存自动失效。 */
export function thumbnailUrl(imageId: number, version: number = THUMB_VERSION): string {
  return `${thumbOrigin}/${imageId}?v=${version}`;
}

/** 开发基准样本；同一 `run` 复用浏览器缓存。 */
export function benchThumbnailUrl(index: number, run: number): string {
  return `${thumbOrigin}/bench/${index}?run=${run}`;
}

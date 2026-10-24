/**
 * 缩略图解码缓存：按 URL 做 LRU，上限来自 `settings.ui.thumbCacheCount`。
 *
 * 缓存里存的是已经加载完成的 `<img>` 元素引用。元素只要还活着，浏览器就会保留它的
 * 解码结果；因此滚回已经看过的区域时，新挂载的 `<img>` 用同一个 URL 能立刻出图，
 * 不再走一次“骨架屏 → 解码 → 淡入”。
 *
 * 淘汰只丢引用、不清 `src`：被淘汰的元素可能正在页面上显示，清空会让可见图片变白。
 * 引用一丢，等 React 卸载该元素后由 GC 回收。
 */

const DEFAULT_LIMIT = 1024;
const MIN_LIMIT = 256;
const MAX_LIMIT = 8192;

const entries = new Map<string, HTMLImageElement>();
let limit = DEFAULT_LIMIT;

export function setThumbCacheLimit(value: number): void {
  const next = Number.isFinite(value) ? Math.round(value) : DEFAULT_LIMIT;
  limit = Math.min(MAX_LIMIT, Math.max(MIN_LIMIT, next));
  trim();
}

/** 命中时把条目移到队尾（最近使用），未命中返回 null。 */
function touchThumb(url: string): HTMLImageElement | null {
  const element = entries.get(url);
  if (!element) return null;
  entries.delete(url);
  entries.set(url, element);
  return element;
}

/** 记录一张已加载完成的缩略图；同一 URL 只保留一个元素。 */
export function rememberThumb(url: string, element: HTMLImageElement): void {
  if (!url) return;
  entries.delete(url);
  entries.set(url, element);
  trim();
}

/** 单张缩略图是否已经有可用（解码完成）的缓存。 */
export function hasDecodedThumb(url: string): boolean {
  const element = touchThumb(url);
  return element != null && element.complete && element.naturalWidth > 0;
}

function trim(): void {
  while (entries.size > limit) {
    const oldest = entries.keys().next();
    if (oldest.done) return;
    entries.delete(oldest.value);
  }
}

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
const browserMock = import.meta.env.DEV && !isTauri();

// WebView2 只接受 http 形式的自定义协议，其它平台用 thumb://。
const thumbOrigin = /Windows/i.test(navigator.userAgent)
  ? "http://thumb.localhost"
  : "thumb://localhost";

/** 缩略图版本回退值，与 schema 的 `thumb_version` 默认值一致；参与 URL 缓存失效。 */
export const THUMB_VERSION = 1;

/**
 * 当前图库的实例 id（manifest 里的 UUID）。图片 id 只是库内代理键，两个库都可能有
 * id=1、version=1，只靠 id 与版本会让 URL 完全相同：浏览器按 `immutable` 长期缓存、
 * 解码缓存也按 URL 命中，切库后就会拿到上一个库的图。所以库 id 必须进 URL。
 *
 * 由 `AppRuntimeProvider` 在渲染子组件之前写入，保证首次出图时已经带上。
 */
let libraryId = "";

export function setThumbLibraryId(id: string | null | undefined): void {
  libraryId = typeof id === "string" ? id : "";
}

function mockThumbnail(imageId: number): string {
  const hue = (imageId * 37) % 360;
  const accent = (hue + 48) % 360;
  const shape =
    imageId % 3 === 0
      ? `<circle cx="265" cy="105" r="78" fill="hsl(${accent} 55% 78% / 0.85)"/>`
      : imageId % 3 === 1
        ? `<rect x="186" y="42" width="132" height="132" rx="18" fill="hsl(${accent} 55% 76% / 0.82)"/>`
        : `<path d="M170 200 L250 62 L330 200 Z" fill="hsl(${accent} 50% 74% / 0.82)"/>`;
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="400" height="300" viewBox="0 0 400 300">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="hsl(${hue} 46% 89%)"/><stop offset="1" stop-color="hsl(${hue} 38% 70%)"/>
</linearGradient></defs>
<rect width="400" height="300" fill="url(#g)"/>
<path d="M0 248 Q120 196 214 242 T400 214 V300 H0 Z" fill="hsl(${hue} 34% 58% / 0.35)"/>
${shape}
</svg>`;
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}

/** 图库缩略图 URL；`version` 变化时浏览器缓存自动失效，`retry` 用于手动重试。 */
export function thumbnailUrl(
  imageId: number,
  version: number = THUMB_VERSION,
  retry = 0,
): string {
  if (browserMock) return mockThumbnail(imageId);
  const url = `${thumbOrigin}/${imageId}?v=${version}&lib=${libraryId}`;
  return retry > 0 ? `${url}&retry=${retry}` : url;
}

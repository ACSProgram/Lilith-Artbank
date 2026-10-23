const isWindows = typeof navigator !== "undefined" && /Windows/i.test(navigator.userAgent);
// WebView2 只接受 http 形式的自定义协议，其它平台用 img://。
const origin = isWindows ? "http://img.localhost" : "img://localhost";

/** 原图或降采样预览 URL；`v` 使用内容哈希，保存后自动失效。重试参数由 `withRetry` 追加。 */
export function imageUrl(imageId: number, contentHash: string, preview: boolean): string {
  return `${origin}${preview ? "/preview" : ""}/${imageId}?v=${contentHash}`;
}

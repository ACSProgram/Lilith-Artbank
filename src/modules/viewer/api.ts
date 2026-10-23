import { invoke } from "@tauri-apps/api/core";
import { imageUrl } from "../../shared/imgUrl";
import type {
  PoseData,
  SaveEditRequest,
  SaveEditResult,
  ViewerFallback,
  ViewerSource,
} from "./types";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

function mockSource(imageId: number, fallback: ViewerFallback): ViewerSource {
  const { width, height } = fallback;
  const hue = (imageId * 37) % 360;
  const accent = (hue + 48) % 360;
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="hsl(${hue} 46% 90%)"/><stop offset="1" stop-color="hsl(${hue} 40% 66%)"/>
</linearGradient></defs>
<rect width="${width}" height="${height}" fill="url(#g)"/>
<path d="M0 ${height * 0.82} Q${width * 0.3} ${height * 0.64} ${width * 0.55} ${height * 0.8} T${width} ${height * 0.72} V${height} H0 Z" fill="hsl(${hue} 34% 54% / 0.36)"/>
<circle cx="${width * 0.66}" cy="${height * 0.34}" r="${Math.min(width, height) * 0.17}" fill="hsl(${accent} 55% 78% / 0.85)"/>
<text x="${width * 0.04}" y="${height * 0.95}" fill="hsl(${hue} 30% 30% / 0.6)" font-family="sans-serif" font-size="${Math.max(12, Math.min(width, height) * 0.03)}">MOCK #${imageId}</text>
</svg>`;
  return {
    imageId,
    contentHash: fallback.contentHash,
    fileExt: fallback.fileExt,
    format: fallback.format,
    fileSize: fallback.fileSize,
    thumbVersion: fallback.thumbVersion,
    hasAlpha: fallback.hasAlpha,
    width,
    height,
    displayWidth: width,
    displayHeight: height,
    preview: false,
    downsampled: false,
    editable: fallback.deletedAt == null,
    deleted: fallback.deletedAt != null,
    maxEditPixels: 50_000_000,
    url: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`,
  };
}

export const viewerApi = {
  async getViewerSource(imageId: number, fallback: ViewerFallback): Promise<ViewerSource> {
    if (!isTauri()) return mockSource(imageId, fallback);
    const source = await invoke<Omit<ViewerSource, "url">>("get_viewer_source", { imageId });
    return {
      ...source,
      url: imageUrl(source.imageId, source.contentHash, source.preview),
    };
  },

  async saveImageEdit(request: SaveEditRequest): Promise<SaveEditResult> {
    if (!isTauri()) {
      throw new Error("浏览器预览不会写入图库；请在 Tauri 窗口中保存编辑。");
    }
    return invoke<SaveEditResult>("save_image_edit", { request });
  },

  async copyImage(imageId: number): Promise<void> {
    if (!isTauri()) {
      throw new Error("浏览器预览无法复制图片到剪贴板");
    }
    return invoke("copy_image", { imageId });
  },

  async getImagePose(imageId: number): Promise<PoseData | null> {
    if (!isTauri()) return null;
    return invoke<PoseData | null>("get_image_pose", { imageId });
  },

  async toggleFullscreen(): Promise<boolean> {
    if (!isTauri()) {
      if (document.fullscreenElement) {
        await document.exitFullscreen();
        return false;
      }
      await document.documentElement.requestFullscreen();
      return true;
    }
    return invoke<boolean>("window_toggle_fullscreen");
  },
};

export function withRetry(url: string, retry: number): string {
  if (retry <= 0) return url;
  return `${url}${url.includes("?") ? "&" : "?"}retry=${retry}`;
}

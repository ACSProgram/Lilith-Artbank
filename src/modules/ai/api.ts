import { invoke } from "@tauri-apps/api/core";
import type {
  AiDescriptionResult,
  AiStatus,
  ImageSimilarityResult,
  PoseData,
  SemanticSearchResult,
  SimilarityPage,
} from "./types";
import type { SimilarKind } from "../../shared/similarity";
import type { TaskScope } from "../../shared/scope";

export const aiBrowserPreview =
  typeof window === "undefined" || !("__TAURI_INTERNALS__" in window);

function requireTauri(action: string): void {
  if (aiBrowserPreview) throw new Error(`${action}需要 Tauri 桌面窗口`);
}

/** 浏览器预览下的离线状态；每次返回新对象，避免调用方改动共享常量。 */
function offlineStatus(): AiStatus {
  return {
    baseUrl: "http://127.0.0.1:18766",
    autoStart: false,
    launchConfigured: false,
    online: false,
    process: {
      state: "stopped",
      owned: false,
      message: "浏览器预览不会连接 AI 服务",
      attempts: 0,
    },
    health: null,
    mean: {
      phase: "missing",
      model: null,
      dim: null,
      count: 0,
      message: "浏览器预览没有图库 embedding",
    },
  };
}

export const aiApi = {
  async getStatus(): Promise<AiStatus> {
    if (aiBrowserPreview) return offlineStatus();
    return invoke<AiStatus>("get_ai_status");
  },

  async startService(): Promise<void> {
    requireTauri("进程托管");
    return invoke("ai_start_service");
  },

  async stopService(): Promise<void> {
    requireTauri("进程托管");
    return invoke("ai_stop_service");
  },

  async describeImage(
    imageId: number,
    prompt?: string,
    maxTokens?: number,
  ): Promise<AiDescriptionResult> {
    requireTauri("AI 描述");
    return invoke<AiDescriptionResult>("ai_describe_image", {
      imageId,
      prompt: prompt ?? null,
      maxTokens: maxTokens ?? null,
    });
  },

  async generatePose(imageId: number): Promise<PoseData> {
    requireTauri("姿势识别");
    return invoke<PoseData>("ai_generate_pose", { imageId });
  },

  async semanticSearch(text: string, limit?: number): Promise<SemanticSearchResult> {
    requireTauri("语义搜索");
    return invoke<SemanticSearchResult>("semantic_search", {
      text,
      limit: limit ?? null,
    });
  },

  async findSimilarImages(request: {
    kind: SimilarKind;
    targetIds: number[];
    threshold?: number | null;
    limit?: number | null;
    scope?: TaskScope | null;
  }): Promise<ImageSimilarityResult> {
    requireTauri("相似检索");
    return invoke<ImageSimilarityResult>("find_similar_images", {
      kind: request.kind,
      targetIds: request.targetIds,
      threshold: request.threshold ?? null,
      limit: request.limit ?? null,
      scope: request.scope ?? null,
    });
  },

  async getSimilarityPage<T>(
    token: string,
    offset: number,
    limit?: number,
  ): Promise<SimilarityPage<T>> {
    requireTauri("语义搜索");
    return invoke<SimilarityPage<T>>("get_similarity_page", {
      token,
      offset,
      limit: limit ?? null,
    });
  },

  async getSimilarityIds(token: string): Promise<number[]> {
    requireTauri("语义搜索");
    return invoke<number[]>("get_similarity_ids", { token });
  },
};

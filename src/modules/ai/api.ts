import { invoke } from "@tauri-apps/api/core";
import type {
  AiDescriptionResult,
  AiJobRequest,
  AiJobSnapshot,
  AiStatus,
  ImageSimilarityResult,
  PoseData,
  SemanticSearchResult,
  SimilarityGroupsResult,
  SimilarityPage,
} from "./types";
import type { SimilarGroupKind, SimilarKind } from "../../shared/similarity";

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
    executableConfigured: false,
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
    job: null,
  };
}

export const aiApi = {
  async getStatus(): Promise<AiStatus> {
    if (aiBrowserPreview) return offlineStatus();
    return invoke<AiStatus>("get_ai_status");
  },

  async ensureModels(capabilities: string[]): Promise<AiStatus> {
    requireTauri("加载模型");
    return invoke<AiStatus>("ai_ensure_models", { capabilities });
  },

  async startService(): Promise<void> {
    requireTauri("进程托管");
    return invoke("ai_start_service");
  },

  async stopService(): Promise<void> {
    requireTauri("进程托管");
    return invoke("ai_stop_service");
  },

  async startJob(request: AiJobRequest): Promise<AiJobSnapshot> {
    requireTauri("AI 任务");
    return invoke<AiJobSnapshot>("start_ai_job", { request });
  },

  async cancelJob(jobId: string): Promise<void> {
    if (aiBrowserPreview) return;
    return invoke("cancel_ai_job", { jobId });
  },

  async getJobStatus(jobId: string): Promise<AiJobSnapshot | null> {
    if (aiBrowserPreview) return null;
    return invoke<AiJobSnapshot | null>("get_ai_job_status", { jobId });
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

  async findSimilarImages(
    kind: SimilarKind,
    targetId: number,
    threshold?: number,
    limit?: number,
  ): Promise<ImageSimilarityResult> {
    requireTauri("找相似");
    return invoke<ImageSimilarityResult>("find_similar_images", {
      kind,
      targetId,
      threshold: threshold ?? null,
      limit: limit ?? null,
    });
  },

  async findSimilarGroups(
    kind: SimilarGroupKind,
    threshold?: number,
    similarityThreshold?: number,
  ): Promise<SimilarityGroupsResult> {
    requireTauri("相似分组");
    return invoke<SimilarityGroupsResult>("find_similar_groups", {
      kind,
      threshold: threshold ?? null,
      similarityThreshold: similarityThreshold ?? null,
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

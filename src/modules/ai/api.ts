import { invoke } from "@tauri-apps/api/core";
import type {
  AiDescriptionResult,
  AiJobRequest,
  AiJobSnapshot,
  AiStatus,
  PoseData,
  SemanticSearchResult,
  SimilarityPage,
} from "./types";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

const offlineStatus: AiStatus = {
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

export const aiApi = {
  async getStatus(): Promise<AiStatus> {
    if (!isTauri()) return { ...offlineStatus };
    return invoke<AiStatus>("get_ai_status");
  },

  async ensureModels(capabilities: string[]): Promise<AiStatus> {
    if (!isTauri()) throw new Error("加载模型需要 Tauri 桌面窗口");
    return invoke<AiStatus>("ai_ensure_models", { capabilities });
  },

  async startService(): Promise<void> {
    if (!isTauri()) throw new Error("进程托管需要 Tauri 桌面窗口");
    return invoke("ai_start_service");
  },

  async stopService(): Promise<void> {
    if (!isTauri()) throw new Error("进程托管需要 Tauri 桌面窗口");
    return invoke("ai_stop_service");
  },

  async startJob(request: AiJobRequest): Promise<AiJobSnapshot> {
    if (!isTauri()) throw new Error("AI 任务需要 Tauri 桌面窗口");
    return invoke<AiJobSnapshot>("start_ai_job", { request });
  },

  async cancelJob(jobId: string): Promise<void> {
    if (!isTauri()) return;
    return invoke("cancel_ai_job", { jobId });
  },

  async getJobStatus(jobId: string): Promise<AiJobSnapshot | null> {
    if (!isTauri()) return null;
    return invoke<AiJobSnapshot | null>("get_ai_job_status", { jobId });
  },

  async describeImage(
    imageId: number,
    prompt?: string,
    maxTokens?: number,
  ): Promise<AiDescriptionResult> {
    if (!isTauri()) throw new Error("AI 描述需要 Tauri 桌面窗口");
    return invoke<AiDescriptionResult>("ai_describe_image", {
      imageId,
      prompt: prompt ?? null,
      maxTokens: maxTokens ?? null,
    });
  },

  async generatePose(imageId: number): Promise<PoseData> {
    if (!isTauri()) throw new Error("姿势识别需要 Tauri 桌面窗口");
    return invoke<PoseData>("ai_generate_pose", { imageId });
  },

  async semanticSearch(text: string, limit?: number): Promise<SemanticSearchResult> {
    if (!isTauri()) throw new Error("语义搜索需要 Tauri 桌面窗口");
    return invoke<SemanticSearchResult>("semantic_search", {
      text,
      limit: limit ?? null,
    });
  },

  async getSimilarityPage<T>(
    token: string,
    offset: number,
    limit?: number,
  ): Promise<SimilarityPage<T>> {
    if (!isTauri()) throw new Error("语义搜索需要 Tauri 桌面窗口");
    return invoke<SimilarityPage<T>>("get_similarity_page", {
      token,
      offset,
      limit: limit ?? null,
    });
  },

  async getSimilarityIds(token: string): Promise<number[]> {
    if (!isTauri()) throw new Error("语义搜索需要 Tauri 桌面窗口");
    return invoke<number[]>("get_similarity_ids", { token });
  },
};

export const aiBrowserPreview = !isTauri();

import { invoke } from "@tauri-apps/api/core";
import type { SimilarityGroupsResult } from "../modules/ai/types";
import type {
  BatchTaskPreview,
  BatchTaskPreviewRequest,
  BatchTaskRequest,
  OptimizeEstimate,
  TaskSnapshot,
  TaskStartResult,
} from "./taskTypes";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export const taskApi = {
  async start(request: BatchTaskRequest): Promise<TaskStartResult> {
    if (!isTauri()) throw new Error("批量任务需要 Tauri 桌面窗口");
    return invoke<TaskStartResult>("start_batch_task", { request });
  },

  async previewBatch(request: BatchTaskPreviewRequest): Promise<BatchTaskPreview> {
    if (!isTauri()) throw new Error("批量任务需要 Tauri 桌面窗口");
    return invoke<BatchTaskPreview>("preview_batch_task", { request });
  },

  async list(): Promise<TaskSnapshot[]> {
    if (!isTauri()) return [];
    return invoke<TaskSnapshot[]>("list_tasks");
  },

  async cancel(taskId: string): Promise<void> {
    if (!isTauri()) return;
    return invoke("cancel_task", { taskId });
  },

  async similarityGroupsResult(taskId: string): Promise<SimilarityGroupsResult | null> {
    if (!isTauri()) return null;
    return invoke<SimilarityGroupsResult | null>("get_similarity_groups_result", { taskId });
  },

  async optimizeEstimate(taskId: string): Promise<OptimizeEstimate | null> {
    if (!isTauri()) return null;
    return invoke<OptimizeEstimate | null>("get_optimize_estimate", { taskId });
  },
};

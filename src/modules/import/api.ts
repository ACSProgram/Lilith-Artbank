import { invoke } from "@tauri-apps/api/core";
import type { ImportRequest, ImportSnapshot } from "./types";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export const importApi = {
  async start(request: ImportRequest): Promise<ImportSnapshot> {
    if (!isTauri()) throw new Error("导入只在 Tauri 桌面窗口中可用");
    return invoke<ImportSnapshot>("start_import", { request });
  },

  async cancel(jobId: string): Promise<void> {
    if (!isTauri()) return;
    return invoke<void>("cancel_import", { jobId });
  },

  async status(jobId: string): Promise<ImportSnapshot | null> {
    if (!isTauri()) return null;
    return invoke<ImportSnapshot | null>("get_import_status", { jobId });
  },
};

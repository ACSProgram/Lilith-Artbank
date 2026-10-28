import { invoke } from "@tauri-apps/api/core";
import type { ImportOptimize, ImportRequest, ImportSnapshot } from "./types";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** `get_settings` 里与导入有关的那一小块；整份设置不归导入模块管。 */
interface SettingsImportShape {
  import?: { optimize?: ImportOptimize | null } | null;
}

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

  /** 上次用的「导入时压缩」选项；没存过或读不到都按关闭处理，不弹错。 */
  async readOptimizeOption(): Promise<ImportOptimize | null> {
    if (!isTauri()) return null;
    try {
      const settings = await invoke<SettingsImportShape>("get_settings");
      return settings?.import?.optimize ?? null;
    } catch {
      return null;
    }
  },

  /** 记住「导入时压缩」；关闭时传 `null`。 */
  async saveOptimizeOption(option: ImportOptimize | null): Promise<void> {
    if (!isTauri()) return;
    await invoke("set_import_optimize", { option });
  },
};

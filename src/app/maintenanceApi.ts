import { invoke } from "@tauri-apps/api/core";
import type { TaskStartResult } from "./taskTypes";
import type { InspectionReport, PackStats, ReportGroup } from "./maintenanceTypes";

const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/**
 * 维护页专用的少量命令。
 *
 * 检查与导出走 `start_batch_task`（范围 + 选项的形状与批量任务一致），
 * 包回收走专用的 `start_pack_compact`；这里只留「读包统计」「立即回收」「取报告」「导出报告」「移孤儿」五件事。
 */
export const maintenanceApi = {
  async packStats(): Promise<PackStats> {
    if (!isTauri()) throw new Error("维护功能需要 Tauri 桌面窗口");
    return invoke<PackStats>("get_pack_stats");
  },

  /** 立即回收：不等空闲条件，但仍然排队等其它写任务。 */
  async compactNow(): Promise<TaskStartResult> {
    if (!isTauri()) throw new Error("维护功能需要 Tauri 桌面窗口");
    return invoke<TaskStartResult>("start_pack_compact");
  },

  async inspectionReport(taskId: string): Promise<InspectionReport | null> {
    if (!isTauri()) return null;
    return invoke<InspectionReport | null>("get_inspection_report", { taskId });
  },

  /**
   * 报告导出 CSV：后端直接写文件，几万行不经过 IPC。
   *
   * `group` 为空时四组全写；给了 `missing / orphan / sizeMismatch / hashMismatch`
   * 就只写那一组，对应界面上的「导出这一组」。
   */
  async saveInspectionReport(taskId: string, path: string, group?: ReportGroup): Promise<void> {
    if (!isTauri()) throw new Error("维护功能需要 Tauri 桌面窗口");
    return invoke("save_inspection_report", { taskId, path, group });
  },

  /** 基于当次检查报告把孤儿移入回收站；返回的任务带可撤销的操作记录。 */
  async moveOrphans(reportTaskId: string, paths: string[]): Promise<TaskStartResult> {
    if (!isTauri()) throw new Error("维护功能需要 Tauri 桌面窗口");
    return invoke<TaskStartResult>("move_orphans", { reportTaskId, paths });
  },
};

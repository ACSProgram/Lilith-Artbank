import { createContext, useContext } from "react";
import type { BatchTaskRequest, TaskSnapshot, TaskStartResult } from "./taskTypes";

export interface TaskCenter {
  tasks: TaskSnapshot[];
  active: TaskSnapshot[];
  /** 任务改变图库数据时递增，页面据此刷新。 */
  dataVersion: number;
  start: (request: BatchTaskRequest) => Promise<TaskStartResult>;
  cancel: (taskId: string) => Promise<void>;
  find: (taskId: string) => TaskSnapshot | null;
  /** 最近结束的任务；用于撤销后关闭查看器等一次性收尾。 */
  lastFinished: TaskSnapshot | null;
}

export const TaskCenterContext = createContext<TaskCenter | null>(null);

export function useTaskCenter(): TaskCenter {
  const value = useContext(TaskCenterContext);
  if (!value) throw new Error("useTaskCenter 必须在 TaskCenterProvider 内使用");
  return value;
}

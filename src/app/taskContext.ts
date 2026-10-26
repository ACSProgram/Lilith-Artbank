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

/**
 * 只关心“有没有任务在跑”的窄上下文。
 *
 * 任务快照每 150ms 变一次；网格和浏览界面订阅整包 `tasks` 会跟着重渲染，
 * 所以这里只暴露计数与标题。
 */
export interface TaskActiveInfo {
  count: number;
  label: string | null;
}

export const TaskActiveContext = createContext<TaskActiveInfo>({ count: 0, label: null });

export function useTaskActive(): TaskActiveInfo {
  return useContext(TaskActiveContext);
}

/** 图库数据版本与最近结束的任务；浏览界面只订阅这两项。 */
export interface TaskDataInfo {
  dataVersion: number;
  lastFinished: TaskSnapshot | null;
}

export const TaskDataContext = createContext<TaskDataInfo>({
  dataVersion: 0,
  lastFinished: null,
});

export function useTaskData(): TaskDataInfo {
  return useContext(TaskDataContext);
}

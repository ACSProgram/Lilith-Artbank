import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { taskApi } from "./taskApi";
import { TaskCenterContext, type TaskCenter } from "./taskContext";
import { TaskCapsule } from "./TaskCapsule";
import { isTaskActive, type BatchTaskRequest, type TaskSnapshot } from "./taskTypes";
import { errorMessage } from "../shared/errors";
import { useNotices } from "./noticeContext";

const MAX_TASKS = 50;

export function TaskCenterProvider({ children }: { children: ReactNode }) {
  const { notify } = useNotices();
  const [tasks, setTasks] = useState<TaskSnapshot[]>([]);
  const [dataVersion, setDataVersion] = useState(0);
  const [lastFinished, setLastFinished] = useState<TaskSnapshot | null>(null);
  const tasksRef = useRef(tasks);
  tasksRef.current = tasks;

  const merge = useCallback((snapshot: TaskSnapshot) => {
    setTasks((previous) => {
      const index = previous.findIndex((task) => task.taskId === snapshot.taskId);
      const next =
        index >= 0
          ? previous.map((task) => (task.taskId === snapshot.taskId ? snapshot : task))
          : [snapshot, ...previous];
      return next.slice(0, MAX_TASKS);
    });
  }, []);

  useEffect(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];

    void taskApi.list().then((items) => {
      if (!disposed) setTasks(items);
    });

    void (async () => {
      if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) return;
      const { listen } = await import("@tauri-apps/api/event");
      if (disposed) return;
      const progress = await listen<TaskSnapshot>("task-progress", (event) => {
        if (disposed) return;
        merge(event.payload);
      });
      const finished = await listen<TaskSnapshot>("task-finished", (event) => {
        if (disposed) return;
        merge(event.payload);
        setLastFinished(event.payload);
        // 相似分组与空间优化估算不改图库数据，不需要刷新浏览页面。
        if (
          event.payload.kind !== "similarityGroups" &&
          event.payload.kind !== "optimizeEstimate"
        ) {
          setDataVersion((value) => value + 1);
        }
      });
      unlisteners.push(progress, finished);
    })();

    return () => {
      disposed = true;
      for (const unlisten of unlisteners) unlisten();
    };
  }, [merge]);

  const start = useCallback(
    async (request: BatchTaskRequest) => {
      const result = await taskApi.start(request);
      if (result.status === "started") merge(result.task);
      return result;
    },
    [merge],
  );

  const cancel = useCallback(
    async (taskId: string) => {
      try {
        await taskApi.cancel(taskId);
      } catch (cause) {
        notify("取消任务失败", "error", errorMessage(cause));
      }
    },
    [notify],
  );

  const find = useCallback(
    (taskId: string) => tasksRef.current.find((task) => task.taskId === taskId) ?? null,
    [],
  );

  const active = useMemo(() => tasks.filter(isTaskActive), [tasks]);

  const value = useMemo<TaskCenter>(
    () => ({ tasks, active, dataVersion, start, cancel, find, lastFinished }),
    [tasks, active, dataVersion, start, cancel, find, lastFinished],
  );

  return (
    <TaskCenterContext.Provider value={value}>
      {children}
      <TaskCapsule />
    </TaskCenterContext.Provider>
  );
}

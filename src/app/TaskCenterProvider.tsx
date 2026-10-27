import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { taskApi } from "./taskApi";
import {
  TaskActiveContext,
  TaskCenterContext,
  TaskDataContext,
  type TaskActiveInfo,
  type TaskCenter,
  type TaskChangeInfo,
  type TaskDataInfo,
} from "./taskContext";
import { TaskCapsule } from "./TaskCapsule";
import { isTaskActive, type BatchTaskRequest, type TaskSnapshot } from "./taskTypes";
import { errorMessage } from "../shared/errors";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import { McpApprovalHost } from "./McpApprovalCard";
import {
  dataChangeForMcp,
  dataChangeForTaskKind,
  type McpChangePayload,
} from "../modules/library/dataChange";

const MAX_TASKS = 50;

export function TaskCenterProvider({ children }: { children: ReactNode }) {
  const { notify } = useNotices();
  const { settings } = useAppRuntime();
  const [tasks, setTasks] = useState<TaskSnapshot[]>([]);
  const [dataVersion, setDataVersion] = useState(0);
  const [lastFinished, setLastFinished] = useState<TaskSnapshot | null>(null);
  const [lastChange, setLastChange] = useState<TaskChangeInfo | null>(null);
  const changeRevision = useRef(0);
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
          const change = dataChangeForTaskKind(event.payload.kind);
          if (change.kind !== "none") {
            changeRevision.current += 1;
            setLastChange({
              revision: changeRevision.current,
              change,
              closeViewer: false,
            });
          }
        }
      });
      // MCP 同步写入不产生任务，后端单独发 library-changed；同样推进数据版本，
      // 让网格、文件夹、计数与打开着的操作记录面板一起刷新。
      const changed = await listen<McpChangePayload & { closeViewer?: boolean }>(
        "library-changed",
        (event) => {
          if (disposed) return;
          setDataVersion((value) => value + 1);
          const change = dataChangeForMcp(event.payload);
          if (change.kind !== "none") {
            changeRevision.current += 1;
            setLastChange({
              revision: changeRevision.current,
              change,
              closeViewer: event.payload.closeViewer === true,
            });
          }
        },
      );
      unlisteners.push(progress, finished, changed);
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

  // 任务进度每 150ms 变一次；这两个窄上下文只在真正相关的值变化时才换新对象，
  // 让网格与浏览界面不必跟着整包 `tasks` 重渲染。
  const activeInfo = useMemo<TaskActiveInfo>(
    () => ({ count: active.length, label: active[0]?.label ?? null }),
    [active],
  );
  const dataInfo = useMemo<TaskDataInfo>(
    () => ({ dataVersion, lastFinished, lastChange }),
    [dataVersion, lastFinished, lastChange],
  );

  return (
    <TaskCenterContext.Provider value={value}>
      <TaskActiveContext.Provider value={activeInfo}>
        <TaskDataContext.Provider value={dataInfo}>
          {children}
        </TaskDataContext.Provider>
      </TaskActiveContext.Provider>
      <McpApprovalHost enabled={Boolean(settings?.mcp.enabled)} />
      <TaskCapsule />
    </TaskCenterContext.Provider>
  );
}

import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronDown, ChevronUp, Square, X } from "lucide-react";
import { Button, IconButton } from "../shared/components/Button";
import { Spinner } from "../shared/components/Spinner";
import { formatDuration } from "../shared/format";
import { useTaskCenter } from "./taskContext";
import {
  CANCEL_PENDING_NOTE,
  TASK_PHASE_LABELS,
  isTaskActive,
  taskPercent,
  type TaskSnapshot,
} from "./taskTypes";

/** 已运行时长；与 ETA 的“还剩多久”分开，一个带秒、一个取整到分钟。 */
function formatElapsed(startedAt: number, finishedAt: number | null): string {
  const end = finishedAt ?? Date.now();
  const seconds = Math.max(0, Math.round((end - startedAt) / 1000));
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes} 分 ${seconds % 60} 秒`;
}

function TaskRow({
  task,
  onCancel,
}: {
  task: TaskSnapshot;
  onCancel: (taskId: string) => void;
}) {
  const [showFailures, setShowFailures] = useState(false);
  const active = isTaskActive(task);
  const cancelling = task.phase === "cancelling";
  const percent = taskPercent(task);
  const done = task.completed + task.failed + task.skipped;

  return (
    <div className={`task-row${active ? " is-active" : ""}`}>
      <div className="task-row-head">
        <div className="task-row-title">
          {active ? <Spinner size={12} /> : null}
          <strong>{task.label}</strong>
          <span className="task-row-scope">{task.scope}</span>
        </div>
        <span className={`task-phase task-phase-${task.phase}`}>
          {TASK_PHASE_LABELS[task.phase]}
        </span>
      </div>

      {task.total > 0 || task.progressPercent !== null ? (
        <div className={`task-progress${cancelling ? " is-cancelling" : ""}`}>
          <span style={{ width: `${percent}%` }} />
        </div>
      ) : null}

      <div className="task-row-meta">
        <span>
          {task.total > 0
            ? `${done.toLocaleString()} / ${task.total.toLocaleString()}`
            : task.message || "—"}
          {task.failed > 0 ? ` · 失败 ${task.failed.toLocaleString()}` : ""}
          {task.skipped > 0 ? ` · 跳过 ${task.skipped.toLocaleString()}` : ""}
          {active && task.etaSeconds != null
            ? ` · 还需 ${formatDuration(task.etaSeconds)}`
            : ""}
        </span>
        <span>{formatElapsed(task.startedAt, task.finishedAt)}</span>
      </div>

      {cancelling ? (
        <div className="task-row-note">{CANCEL_PENDING_NOTE}</div>
      ) : active && task.message && task.total > 0 ? (
        <div className="task-row-message">{task.message}</div>
      ) : null}

      <div className="task-row-actions">
        {task.failures.length > 0 ? (
          <Button
            size="sm"
            variant="ghost"
            icon={showFailures ? <ChevronUp size={12} /> : <ChevronDown size={12} />}
            onClick={() => setShowFailures((value) => !value)}
          >
            失败 {task.failures.length} 条
          </Button>
        ) : null}
        {active && task.cancellable ? (
          <Button
            size="sm"
            variant="danger"
            icon={<Square size={11} />}
            disabled={cancelling}
            onClick={() => onCancel(task.taskId)}
          >
            取消任务
          </Button>
        ) : null}
      </div>

      {showFailures ? (
        <div className="task-failures">
          {task.failures.slice(0, 20).map((failure, index) => (
            <div key={`${failure.imageId ?? failure.path}-${index}`}>
              <span>{failure.imageId ? `#${failure.imageId}` : failure.path || "任务"}</span>
              <em>{failure.reason}</em>
            </div>
          ))}
          {task.failures.length > 20 ? (
            <span className="task-failures-more">仅显示前 20 条</span>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

export function TaskCapsule() {
  const { tasks, active, cancel } = useTaskCenter();
  const [open, setOpen] = useState(false);
  // 任务刚结束时保留入口一段时间，让用户能看到失败清单。
  const [showAfterFinish, setShowAfterFinish] = useState(false);
  const hadActive = useRef(false);

  useEffect(() => {
    if (active.length > 0) {
      hadActive.current = true;
      setShowAfterFinish(true);
      return;
    }
    if (!hadActive.current) return;
    const timer = window.setTimeout(() => {
      hadActive.current = false;
      setShowAfterFinish(false);
    }, 12_000);
    return () => window.clearTimeout(timer);
  }, [active.length]);

  const ordered = useMemo(
    () =>
      [...tasks].sort((left, right) => {
        const activeDelta =
          Number(isTaskActive(right)) - Number(isTaskActive(left));
        return activeDelta !== 0 ? activeDelta : right.startedAt - left.startedAt;
      }),
    [tasks],
  );

  if (tasks.length === 0) return null;
  if (active.length === 0 && !open && !showAfterFinish) return null;

  const percent =
    active.length > 0
      ? Math.round(
          active.reduce((sum, task) => sum + taskPercent(task), 0) / active.length,
        )
      : 100;

  return (
    <div className="task-center">
      {open ? (
        <section className="task-panel" aria-label="后台任务">
          <header className="task-panel-head">
            <strong>任务</strong>
            <IconButton label="收起任务面板" onClick={() => setOpen(false)}>
              <X size={14} />
            </IconButton>
          </header>
          <div className="task-panel-body">
            {ordered.length === 0 ? (
              <p className="task-empty">还没有任务</p>
            ) : (
              ordered.map((task) => (
                <TaskRow
                  key={task.taskId}
                  task={task}
                  onCancel={(taskId) => void cancel(taskId)}
                />
              ))
            )}
          </div>
        </section>
      ) : null}

      <button
        type="button"
        className={`task-pill${active.length > 0 ? " is-active" : ""}`}
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
      >
        {active.length > 0 ? <Spinner size={12} /> : null}
        {active.length > 0
          ? `${active.length} 个任务 · ${percent}%`
          : `任务记录 ${tasks.length}`}
      </button>
    </div>
  );
}

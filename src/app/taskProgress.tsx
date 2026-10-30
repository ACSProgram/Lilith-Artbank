import type { TaskSnapshot } from "./taskTypes";
import { CANCEL_PENDING_NOTE, isTaskActive, taskPercent } from "./taskTypes";

export interface TaskProgressState {
  active: boolean;
  cancelling: boolean;
  /** 已完成 + 失败 + 跳过；与任务快照的“已结算张数”同口径。 */
  done: number;
  percent: number;
}

export function taskProgressState(task: TaskSnapshot): TaskProgressState {
  return {
    active: isTaskActive(task),
    cancelling: task.phase === "cancelling",
    done: task.completed + task.failed + task.skipped,
    percent: taskPercent(task),
  };
}

/** 各对话框与任务胶囊共用的进度条；取消中统一转斜纹。 */
export function TaskProgressBar({
  percent,
  cancelling = false,
  className,
}: {
  percent: number;
  cancelling?: boolean;
  className?: string;
}) {
  return (
    <div
      className={["task-progress", cancelling && "is-cancelling", className]
        .filter(Boolean)
        .join(" ")}
      aria-hidden="true"
    >
      <span style={{ width: `${percent}%` }} />
    </div>
  );
}

/** 取消中说明：等当前批次提交完成，已执行的部分不会撤销。 */
export function CancelPendingNote({ visible }: { visible: boolean }) {
  if (!visible) return null;
  return <div className="task-cancel-note">{CANCEL_PENDING_NOTE}</div>;
}

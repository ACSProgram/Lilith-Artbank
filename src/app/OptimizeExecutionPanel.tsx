import { Spinner } from "../shared/components/Spinner";
import { formatBytes, formatDuration, formatRate } from "../shared/format";
import { compareOptimizeEstimate, type OptimizeComparisonKey } from "./optimizeProgress";
import {
  TASK_PHASE_LABELS,
  isTaskActive,
  settledRatio,
  type OptimizeEstimate,
  type TaskSnapshot,
} from "./taskTypes";
import { CancelPendingNote, TaskProgressBar } from "./taskProgress";

/** 估算统计候选时没有可按张计的进度，只留百分比与文案。 */
export function ProgressPanel({ task, percent }: { task: TaskSnapshot; percent: number }) {
  const counted = task.total > 0;
  return (
    <div className="batch-task-progress">
      <div className="batch-task-progress-head">
        <strong>
          {task.label} · {TASK_PHASE_LABELS[task.phase]}
        </strong>
        {counted ? (
          <span>
            {(task.completed + task.failed + task.skipped).toLocaleString()} /{" "}
            {task.total.toLocaleString()}
          </span>
        ) : (
          <span>{percent}%</span>
        )}
      </div>
      <TaskProgressBar percent={percent} />
      <div className="batch-task-progress-meta">
        <span>{task.message || (isTaskActive(task) ? <Spinner size={12} /> : "已结束")}</span>
      </div>
    </div>
  );
}

/**
 * 执行百分比文案：任务没结束就不写 100%，还差几十张时写 100% 会和「预计还需 9 秒」自相矛盾。
 *
 * ≥99% 给一位小数（`99.8%`），低于 10% 也给一位小数——候选十几万张时 1% 也要一千多张。
 */
function percentLabel(percent: number, finished: boolean): string {
  if (finished) return "100%";
  if (percent >= 99) return `${Math.min(99.9, percent).toFixed(1)}%`;
  return percent >= 10 ? `${Math.round(percent)}%` : `${percent.toFixed(1)}%`;
}

function elapsedSecondsOf(task: TaskSnapshot): number {
  // 从真正进入编码算起，不含排队、准备与执行前复核。
  if (task.runStartedAt == null) return 0;
  const end = task.finishedAt ?? Date.now();
  return Math.max(0, (end - task.runStartedAt) / 1000);
}

function comparisonValue(key: OptimizeComparisonKey, value: number): string {
  if (key === "savings") return formatBytes(value);
  if (key === "duration") return formatDuration(value);
  return `${value.toLocaleString()} 张`;
}

/**
 * 执行时只保留这一行状态：进度、已结算张数、已用/剩余时间、速度与当前文件。
 *
 * 「已完成」= 已替换 + 已跳过 + 失败，分母是候选张数；环图中心与曲线横轴读同一个数，
 * 「已替换」只算真正换掉原图的，下沉为环图图例的第一行。
 * 任务结束后换成一行结论，不再显示「预计还需」与速度。
 */
export function OptimizeExecutionStatus({ task }: { task: TaskSnapshot }) {
  const done = task.completed + task.failed + task.skipped;
  // 进度条宽度、百分比文案与环图中心读同一个比值。
  const percent = settledRatio(task) * 100;
  const cancelling = task.phase === "cancelling";
  const finished = task.finishedAt != null;

  return (
    <div className="batch-task-progress optimize-execution">
      <div className="batch-task-progress-head">
        <strong>
          {task.label} · {TASK_PHASE_LABELS[task.phase]}
        </strong>
        <span>{percentLabel(percent, finished)}</span>
      </div>
      <TaskProgressBar percent={percent} cancelling={cancelling} />

      {finished ? (
        <div className="batch-task-facts">
          <span>已替换 {task.completed.toLocaleString()} 张</span>
          <span>省 {formatBytes(task.savedBytes)}</span>
          <span>用时 {formatDuration(elapsedSecondsOf(task))}</span>
        </div>
      ) : (
        <div className="batch-task-facts">
          <span>
            已完成 {done.toLocaleString()} / {task.total.toLocaleString()}
          </span>
          <span>已用时 {formatDuration(elapsedSecondsOf(task))}</span>
          {!cancelling && task.etaSeconds != null ? (
            <span>预计还需 {formatDuration(task.etaSeconds)}</span>
          ) : null}
          {task.ratePerSecond != null ? (
            <span>{formatRate(task.ratePerSecond)} 张/秒</span>
          ) : null}
          {task.currentFile ? (
            <span className="batch-task-current" title={task.currentFile}>
              当前：{task.currentFile}
            </span>
          ) : task.currentImageId ? (
            <span>当前：#{task.currentImageId}</span>
          ) : null}
        </div>
      )}

      {cancelling ? (
        <CancelPendingNote visible />
      ) : task.message && !(finished && task.phase === "completed") ? (
        // 成功结束时顶部已经是结论行，同一句话不再重复；取消与失败仍要说明原因。
        <div className="batch-task-progress-meta">
          <span>{task.message}</span>
        </div>
      ) : null}
    </div>
  );
}

/** 结束后的实际 / 预计对照：耗时、处理张数、节省空间各一句。 */
export function comparisonLine(task: TaskSnapshot, estimate: OptimizeEstimate): string | null {
  if (isTaskActive(task)) return null;
  const comparison = compareOptimizeEstimate(estimate, {
    savedBytes: task.savedBytes,
    elapsedSeconds: elapsedSecondsOf(task),
    completed: task.completed,
    status: task.phase,
  });
  const parts = (["duration", "count", "savings"] as OptimizeComparisonKey[])
    .map((key) => {
      const row = comparison.rows.find((entry) => entry.key === key);
      if (!row) return null;
      return `${row.label} ${comparisonValue(key, row.actual)}（预计 ${comparisonValue(
        key,
        row.estimated,
      )}）`;
    })
    .filter(Boolean);
  if (parts.length === 0) return null;
  return `实际 vs 预计：${parts.join(" · ")}${comparison.completed ? "" : " · 已停止"}`;
}

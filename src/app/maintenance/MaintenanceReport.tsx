import { Download } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { formatBytes } from "../../shared/format";
import {
  REPORT_ITEM_LIMIT,
  reportIsClean,
  type InspectionReport,
  type ReportGroup,
} from "../maintenanceTypes";
import {
  isTaskActive,
  taskPercent,
  TASK_PHASE_LABELS,
  type TaskSnapshot,
} from "../taskTypes";
import { CancelPendingNote, TaskProgressBar } from "../taskProgress";

/** 报告里每张表最多渲染这么多行；再多的部分请导出 CSV。 */
export const REPORT_ROWS = 200;

/** 分组名与 Rust `report_csv_group` 的分组键一致，同时用于导出文件名后缀。 */
export const GROUP_LABELS: Record<ReportGroup, string> = {
  missing: "缺失原图",
  orphan: "孤儿文件",
  sizeMismatch: "大小不符",
  hashMismatch: "哈希不符",
};

/** 任务进度条：与批量任务对话框同一套百分比口径。 */
export function TaskProgress({ task, copiedBytes }: { task: TaskSnapshot; copiedBytes?: number }) {
  const active = isTaskActive(task);
  const cancelling = task.phase === "cancelling";
  const percent = taskPercent(task);
  const note =
    task.message ||
    (task.total > 0
      ? `${task.completed.toLocaleString()} / ${task.total.toLocaleString()}`
      : "准备中…");
  return (
    <div className="maintenance-progress">
      <div className="maintenance-progress-head">
        <span>
          {TASK_PHASE_LABELS[task.phase]} · {task.label}
        </span>
        <span className="list-numeric">
          {active ? `${percent}%` : task.phase === "yielded" ? "已让位" : ""}
        </span>
      </div>
      <TaskProgressBar
        percent={percent}
        cancelling={cancelling}
        className="maintenance-progress-bar"
      />
      <CancelPendingNote visible={cancelling} />
      <p className="maintenance-progress-note">
        {note}
        {copiedBytes ? ` · 已复制 ${formatBytes(copiedBytes)}` : ""}
        {task.failed > 0 ? ` · 失败 ${task.failed.toLocaleString()}` : ""}
      </p>
    </div>
  );
}

/** 报告的四个清单：各自一张表，行数上限一致。 */
export function ReportView({
  report,
  onSave,
}: {
  report: InspectionReport;
  onSave: (group?: ReportGroup) => void;
}) {
  return (
    <section className="dialog-section">
      <div className="maintenance-actions">
        <h3 style={{ margin: 0 }}>{report.mode === "full" ? "完整校验" : "快速检查"}结果</h3>
        <span className="maintenance-summary">
          扫描 {report.scannedRows.toLocaleString()} 行 ·{" "}
          {report.scannedFiles.toLocaleString()} 个文件 · {(report.elapsedMs / 1000).toFixed(1)} 秒
        </span>
        <Button
          variant="secondary"
          icon={<Download size={13} />}
          onClick={() => onSave()}
        >
          导出全部
        </Button>
      </div>
      {report.changedDuringScan > 0 ? (
        <p className="dialog-hint">
          扫描期间有 {report.changedDuringScan.toLocaleString()} 张被改动，已跳过哈希校验；它们不算问题。
        </p>
      ) : null}
      {report.truncated ? (
        <p className="dialog-hint">
          问题太多：每份清单最多保留 {REPORT_ITEM_LIMIT.toLocaleString()} 条，导出 CSV
          拿到的也是这些，超出部分没有记录。
        </p>
      ) : null}
      {reportIsClean(report) ? (
        <p className="dialog-empty">没有发现问题。</p>
      ) : (
        <div className="maintenance-report">
          <ReportTable
            group="missing"
            total={report.missingTotal}
            columns={["路径", "应有大小"]}
            rows={report.missing.map((row) => [row.path, formatBytes(row.expectedSize)])}
            onSave={onSave}
          />
          <ReportTable
            group="orphan"
            total={report.orphanTotal}
            columns={["路径", "大小", "备注"]}
            rows={report.orphans.map((row) => [
              row.path,
              formatBytes(row.size),
              row.otherFolder ? `属于「${row.otherFolder}」` : "库里没有这个哈希",
            ])}
            onSave={onSave}
          />
          <ReportTable
            group="sizeMismatch"
            total={report.sizeMismatchTotal}
            columns={["路径", "记录大小", "实际大小"]}
            rows={report.sizeMismatch.map((row) => [
              row.path,
              formatBytes(row.expectedSize),
              formatBytes(row.actualSize),
            ])}
            onSave={onSave}
          />
          <ReportTable
            group="hashMismatch"
            total={report.hashMismatchTotal}
            columns={["路径"]}
            rows={report.hashMismatch.map((row) => [row.path])}
            onSave={onSave}
          />
        </div>
      )}
    </section>
  );
}

function ReportTable({
  group,
  total,
  columns,
  rows,
  onSave,
}: {
  group: ReportGroup;
  total: number;
  columns: string[];
  rows: string[][];
  onSave: (group?: ReportGroup) => void;
}) {
  if (total === 0) return null;
  const title = GROUP_LABELS[group];
  const shown = rows.slice(0, REPORT_ROWS);
  return (
    <div className="maintenance-report-group">
      <h4>
        {title} <em>{total.toLocaleString()}</em>
        <Button
          variant="ghost"
          size="sm"
          icon={<Download size={12} />}
          style={{ marginLeft: "auto" }}
          onClick={() => onSave(group)}
        >
          导出这一组
        </Button>
      </h4>
      {total > shown.length ? (
        <p className="dialog-hint">
          只显示前 {shown.length.toLocaleString()} 条，共 {total.toLocaleString()} 条；导出 CSV
          可以看全部。
        </p>
      ) : null}
      <div className="maintenance-table-wrap">
        <table className="maintenance-table">
          <thead>
            <tr>
              {columns.map((column, index) => (
                <th key={`${title}-head-${index}`}>{column}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {shown.map((row, rowIndex) => (
              <tr key={`${title}-${rowIndex}`}>
                {row.map((cell, cellIndex) => (
                  <td
                    key={`${title}-${rowIndex}-${cellIndex}`}
                    className={cellIndex === 0 ? "maintenance-path" : "list-numeric"}
                    title={cellIndex === 0 ? cell : undefined}
                  >
                    {cell}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

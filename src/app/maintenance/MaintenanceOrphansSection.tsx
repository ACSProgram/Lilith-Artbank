import { AlertTriangle, Trash2 } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { formatBytes } from "../../shared/format";
import type { InspectionReport } from "../maintenanceTypes";
import type { TaskSnapshot } from "../taskTypes";
import { REPORT_ROWS, TaskProgress } from "./MaintenanceReport";

interface MaintenanceOrphansSectionProps {
  report: InspectionReport | null;
  selectedOrphans: Set<string>;
  onToggleAll: () => void;
  onToggleOrphan: (path: string) => void;
  orphanBusy: boolean;
  orphanActive: boolean;
  orphanTask: TaskSnapshot | null;
  onStartOrphanMove: () => void;
}

export function MaintenanceOrphansSection({
  report,
  selectedOrphans,
  onToggleAll,
  onToggleOrphan,
  orphanBusy,
  orphanActive,
  orphanTask,
  onStartOrphanMove,
}: MaintenanceOrphansSectionProps) {
  return (
    <>
      {report ? (
        <>
          <section className="dialog-section">
            <h3>
              <AlertTriangle size={14} /> 孤儿文件
            </h3>
            <p className="dialog-hint">
              磁盘上有、但数据库里没有登记的文件，只占空间。移走后图库看起来没有任何变化；在「记录」里可以撤销，但清空回收站之后就真的没有了。
            </p>
            <div className="maintenance-actions">
              <Button
                variant="secondary"
                onClick={onToggleAll}
                disabled={report.orphans.length === 0}
              >
                {selectedOrphans.size === report.orphans.length ? "取消全选" : "全选"}
              </Button>
              <Button
                variant="danger"
                icon={<Trash2 size={13} />}
                loading={orphanBusy}
                disabled={selectedOrphans.size === 0 || orphanActive}
                onClick={onStartOrphanMove}
              >
                移入回收站 ({selectedOrphans.size.toLocaleString()})
              </Button>
            </div>
            {orphanTask ? <TaskProgress task={orphanTask} /> : null}
          </section>
          {report.orphans.length === 0 ? (
            <p className="dialog-empty">这份报告里没有孤儿文件。</p>
          ) : (
            <div className="maintenance-table-wrap">
              <table className="maintenance-table">
                <thead>
                  <tr>
                    <th aria-label="选择" />
                    <th>路径</th>
                    <th>大小</th>
                    <th>备注</th>
                  </tr>
                </thead>
                <tbody>
                  {report.orphans.slice(0, REPORT_ROWS).map((orphan) => (
                    <tr key={orphan.path}>
                      <td>
                        <input
                          type="checkbox"
                          checked={selectedOrphans.has(orphan.path)}
                          onChange={() => onToggleOrphan(orphan.path)}
                        />
                      </td>
                      <td className="maintenance-path" title={orphan.path}>
                        {orphan.path}
                      </td>
                      <td className="list-numeric">{formatBytes(orphan.size)}</td>
                      <td>
                        {orphan.otherFolder
                          ? `哈希属于「${orphan.otherFolder}」`
                          : "库里没有这个哈希"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {report.orphans.length > REPORT_ROWS ? (
                <p className="dialog-hint">
                  只显示前 {REPORT_ROWS} 条，共 {report.orphans.length.toLocaleString()}{" "}
                  条；其余请导出 CSV。
                </p>
              ) : null}
            </div>
          )}
        </>
      ) : (
        <p className="dialog-empty">先运行一次检查，这里会列出扫描到的孤儿文件。</p>
      )}
    </>
  );
}

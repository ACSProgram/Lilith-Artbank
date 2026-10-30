import { Download, Square } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { ScopePicker } from "../../shared/components/ScopePicker";
import type { BatchTaskContextInfo } from "../../shared/batchTaskContext";
import type { ActionAvailability } from "../../shared/capability";
import type { ScopeChoice } from "../../shared/scope";
import type { TaskSnapshot } from "../taskTypes";
import { TaskProgress } from "./MaintenanceReport";

interface MaintenanceExportSectionProps {
  context: BatchTaskContextInfo | null;
  exportScope: ScopeChoice;
  onExportScopeChange: (choice: ScopeChoice) => void;
  exportScopeOptions: Record<ScopeChoice, ActionAvailability>;
  exportDir: string | null;
  onPickExportDir: () => void;
  includeManifest: boolean;
  onIncludeManifestChange: (value: boolean) => void;
  starting: boolean;
  exportActive: boolean;
  exportTask: TaskSnapshot | null;
  onStartExport: () => void;
  onCancelExport: () => void;
}

export function MaintenanceExportSection({
  context,
  exportScope,
  onExportScopeChange,
  exportScopeOptions,
  exportDir,
  onPickExportDir,
  includeManifest,
  onIncludeManifestChange,
  starting,
  exportActive,
  exportTask,
  onStartExport,
  onCancelExport,
}: MaintenanceExportSectionProps) {
  return (
    <>
      <section className="dialog-section">
        <h3>范围</h3>
        <ScopePicker
          name="maintenance-export-scope"
          value={exportScope}
          onChange={onExportScopeChange}
          context={context}
          availability={exportScopeOptions}
        />
      </section>
      <section className="dialog-section">
        <h3>目标目录</h3>
        <div className="maintenance-actions">
          <Button variant="secondary" onClick={onPickExportDir}>
            选择目录…
          </Button>
          <span className="maintenance-path" title={exportDir ?? undefined}>
            {exportDir ?? "还没有选择"}
          </span>
        </div>
        <p className="dialog-hint">
          文件按 `内容哈希.扩展名` 复制过去；同名文件直接跳过，不覆盖、不改名。
        </p>
        <label className="toggle-row">
          <input
            type="checkbox"
            checked={includeManifest}
            onChange={(event) => onIncludeManifestChange(event.target.checked)}
          />
          <span>
            <strong>附带 manifest.csv</strong>
            <small>每行一张图：内容哈希、扩展名、宽高、文件夹、评分、描述。</small>
          </span>
        </label>
      </section>
      <section className="dialog-section">
        <div className="maintenance-actions">
          <Button
            variant="primary"
            icon={<Download size={13} />}
            loading={starting}
            disabled={exportActive || !exportDir || !exportScopeOptions[exportScope].enabled}
            onClick={onStartExport}
          >
            开始导出
          </Button>
          {exportActive ? (
            <Button
              variant="danger"
              icon={<Square size={13} />}
              onClick={onCancelExport}
            >
              取消导出
            </Button>
          ) : null}
        </div>
        <p className="dialog-hint">
          导出期间导入、编辑、移动与删除都会被拒绝；全库导出会挡很久。
        </p>
        {exportTask ? (
          <TaskProgress task={exportTask} copiedBytes={exportTask.originalBytes} />
        ) : null}
      </section>
    </>
  );
}

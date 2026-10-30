import { Play, Square } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { ScopePicker } from "../../shared/components/ScopePicker";
import type { BatchTaskContextInfo } from "../../shared/batchTaskContext";
import type { ActionAvailability } from "../../shared/capability";
import type { ScopeChoice } from "../../shared/scope";
import type { InspectionReport, ReportGroup } from "../maintenanceTypes";
import type { TaskSnapshot } from "../taskTypes";
import { ReportView, TaskProgress } from "./MaintenanceReport";

interface MaintenanceCheckSectionProps {
  mode: "fast" | "full";
  onModeChange: (mode: "fast" | "full") => void;
  checkScope: ScopeChoice;
  onCheckScopeChange: (choice: ScopeChoice) => void;
  fullLibrary: boolean;
  onFullLibraryChange: (value: boolean) => void;
  context: BatchTaskContextInfo | null;
  scopeOptions: Record<ScopeChoice, ActionAvailability>;
  checkScopeReady: boolean;
  starting: boolean;
  checkActive: boolean;
  checkTask: TaskSnapshot | null;
  onStartCheck: () => void;
  onCancelCheck: () => void;
  report: InspectionReport | null;
  onSaveReport: (group?: ReportGroup) => void;
}

export function MaintenanceCheckSection({
  mode,
  onModeChange,
  checkScope,
  onCheckScopeChange,
  fullLibrary,
  onFullLibraryChange,
  context,
  scopeOptions,
  checkScopeReady,
  starting,
  checkActive,
  checkTask,
  onStartCheck,
  onCancelCheck,
  report,
  onSaveReport,
}: MaintenanceCheckSectionProps) {
  return (
    <>
      <section className="dialog-section">
        <h3>检查方式</h3>
        <div className="maintenance-choices">
          <button
            type="button"
            className={`maintenance-choice${mode === "fast" ? " is-active" : ""}`}
            onClick={() => onModeChange("fast")}
          >
            <strong>快速检查</strong>
            <small>只看文件在不在、大小对不对。几十万张也就一两分钟。</small>
          </button>
          <button
            type="button"
            className={`maintenance-choice${mode === "full" ? " is-active" : ""}`}
            onClick={() => onModeChange("full")}
          >
            <strong>完整校验</strong>
            <small>在快速检查之上重算 BLAKE3，能发现内容被改过。</small>
          </button>
        </div>
      </section>

      <section className="dialog-section">
        <h3>范围</h3>
        {mode === "full" && fullLibrary ? (
          <p className="dialog-hint">已覆盖整个图库，下面按当前视图的范围不再生效。</p>
        ) : null}
        <fieldset
          className="maintenance-fieldset"
          disabled={mode === "full" && fullLibrary}
        >
          <ScopePicker
            name="maintenance-check-scope"
            value={checkScope}
            onChange={onCheckScopeChange}
            context={context}
            availability={scopeOptions}
          />
        </fieldset>
        {mode === "full" ? (
          <label className="toggle-row">
            <input
              type="checkbox"
              checked={fullLibrary}
              onChange={(event) => onFullLibraryChange(event.target.checked)}
            />
            <span>
              <strong>整个图库</strong>
              <small>
                {fullLibrary
                  ? "开始时会再确认一次：全库完整校验通常要二三十分钟。"
                  : "勾上之后按整个图库算，不受上面的范围限制。"}
              </small>
            </span>
          </label>
        ) : null}
      </section>

      <section className="dialog-section">
        <div className="maintenance-actions">
          <Button
            variant="primary"
            icon={<Play size={13} />}
            loading={starting}
            disabled={checkActive || !checkScopeReady}
            onClick={onStartCheck}
          >
            开始检查
          </Button>
          {checkActive ? (
            <Button
              variant="danger"
              icon={<Square size={13} />}
              onClick={onCancelCheck}
            >
              取消检查
            </Button>
          ) : null}
        </div>
        {checkTask ? <TaskProgress task={checkTask} /> : null}
      </section>

      {report ? (
        <ReportView report={report} onSave={onSaveReport} />
      ) : (
        <p className="dialog-empty">还没有检查结果。运行一次检查后会在这里列出问题。</p>
      )}
    </>
  );
}

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { ClipboardCheck, Download, FileWarning, Package } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Modal } from "../shared/components/Modal";
import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import { errorMessage } from "../shared/errors";
import {
  buildScope,
  defaultScopeChoice,
  scopeAvailability,
  type ScopeChoice,
  type TaskScope,
} from "../shared/scope";
import { useNotices } from "../shared/notices";
import { maintenanceApi } from "./maintenanceApi";
import {
  segmentGarbageRatio,
  type InspectionReport,
  type PackStats,
  type ReportGroup,
} from "./maintenanceTypes";
import { useTaskCenter } from "./taskContext";
import { isTaskActive, type BatchTaskRequest } from "./taskTypes";
import { MaintenanceCheckSection } from "./maintenance/MaintenanceCheckSection";
import { MaintenanceExportSection } from "./maintenance/MaintenanceExportSection";
import { MaintenanceOrphansSection } from "./maintenance/MaintenanceOrphansSection";
import { MaintenancePacksSection } from "./maintenance/MaintenancePacksSection";
import { GROUP_LABELS } from "./maintenance/MaintenanceReport";

type Section = "check" | "orphans" | "packs" | "export";

const SECTIONS: { id: Section; label: string; icon: typeof Package }[] = [
  { id: "check", label: "检查", icon: ClipboardCheck },
  { id: "orphans", label: "孤儿", icon: FileWarning },
  { id: "packs", label: "包", icon: Package },
  { id: "export", label: "导出", icon: Download },
];

interface MaintenanceDialogProps {
  open: boolean;
  onClose: () => void;
  context: BatchTaskContextInfo | null;
  libraryId: string;
}

export function MaintenanceDialog({ open, onClose, context, libraryId }: MaintenanceDialogProps) {
  const { notify, confirm } = useNotices();
  const { start, cancel, find } = useTaskCenter();
  const [section, setSection] = useState<Section>("check");
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  // 检查
  const [mode, setMode] = useState<"fast" | "full">("fast");
  const [checkScope, setCheckScope] = useState<ScopeChoice>("view");
  const [fullLibrary, setFullLibrary] = useState(false);
  const [checkTaskId, setCheckTaskId] = useState<string | null>(null);
  const [report, setReport] = useState<InspectionReport | null>(null);
  const loadedReportFor = useRef<string | null>(null);

  // 孤儿
  const [selectedOrphans, setSelectedOrphans] = useState<Set<string>>(new Set());
  const [orphanTaskId, setOrphanTaskId] = useState<string | null>(null);
  const [orphanBusy, setOrphanBusy] = useState(false);

  // 包
  const [packStats, setPackStats] = useState<PackStats | null>(null);
  const [packStatus, setPackStatus] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [packError, setPackError] = useState<string | null>(null);
  const [packTaskId, setPackTaskId] = useState<string | null>(null);

  // 导出
  const [exportScope, setExportScope] = useState<ScopeChoice>("view");
  const [exportDir, setExportDir] = useState<string | null>(null);
  const [includeManifest, setIncludeManifest] = useState(true);
  const [exportTaskId, setExportTaskId] = useState<string | null>(null);

  const checkTask = checkTaskId ? find(checkTaskId) : null;
  const checkActive = checkTask != null && isTaskActive(checkTask);
  const orphanTask = orphanTaskId ? find(orphanTaskId) : null;
  const orphanActive = orphanTask != null && isTaskActive(orphanTask);
  const packTask = packTaskId ? find(packTaskId) : null;
  const packActive = packTask != null && isTaskActive(packTask);
  const exportTask = exportTaskId ? find(exportTaskId) : null;
  const exportActive = exportTask != null && isTaskActive(exportTask);

  // 换库时丢掉上一库的报告与统计：任务 id 与段号在新库里没有意义。
  useEffect(() => {
    setReport(null);
    setPackStats(null);
    setPackStatus("idle");
    setSelectedOrphans(new Set());
    setCheckTaskId(null);
    setOrphanTaskId(null);
    setPackTaskId(null);
    setExportTaskId(null);
    loadedReportFor.current = null;
  }, [libraryId]);

  // 打开时重置草稿，但保留本次会话里已经扫出来的报告与包统计。
  useEffect(() => {
    if (!open) return;
    setError(null);
    setStarting(false);
    setCheckScope(defaultScopeChoice(context, "view"));
    setExportScope(defaultScopeChoice(context, "view"));
  }, [open, context]);

  // 检查任务结束后取报告：报告按任务 id 存在后端，挤掉时返回空。
  useEffect(() => {
    if (!checkTaskId || !checkTask) return;
    if (isTaskActive(checkTask)) return;
    if (checkTask.phase !== "completed") return;
    if (loadedReportFor.current === checkTaskId) return;
    loadedReportFor.current = checkTaskId;
    void maintenanceApi
      .inspectionReport(checkTaskId)
      .then((next) => {
        if (next) {
          setReport(next);
          setSelectedOrphans(new Set());
        } else {
          setError("报告已被新的检查覆盖，请重新扫描");
        }
      })
      .catch((cause: unknown) => setError(errorMessage(cause)));
  }, [checkTask, checkTaskId]);

  const loadPackStats = useCallback(async () => {
    setPackStatus("loading");
    setPackError(null);
    try {
      const stats = await maintenanceApi.packStats();
      setPackStats(stats);
      setPackStatus("ready");
    } catch (cause) {
      setPackError(errorMessage(cause));
      setPackStatus("error");
    }
  }, []);

  // 打开包分区才查：段统计要遍历段文件大小，不在别的分区白算一遍。
  useEffect(() => {
    if (!open || section !== "packs") return;
    if (packStatus === "loading") return;
    if (packActive) return;
    void loadPackStats();
    // packStatus 故意不进依赖：只在切到分区或任务结束时各查一次。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, section, packActive, loadPackStats]);

  // 回收结束（含让位与取消）后刷新数字。
  const packPhase = packTask?.phase;
  useEffect(() => {
    if (!packPhase) return;
    if (isTaskActive(packTask!)) return;
    void loadPackStats();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [packPhase]);

  // 孤儿移入结束后重新取一次报告：后端已按实际搬走的路径摘过条目，
  // 取消时没轮到的那部分、搬失败的那些都还留在报告里，界面照它显示即可。
  const orphanPhase = orphanTask?.phase;
  const orphanSettled = orphanTask != null && !isTaskActive(orphanTask);
  useEffect(() => {
    if (!orphanSettled || !report) return;
    void maintenanceApi
      .inspectionReport(report.taskId)
      .then((next) => {
        if (next) setReport(next);
      })
      .catch(() => {
        // 取不到就留着当前报告：条目还在，用户最多重新扫一次。
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [orphanPhase]);

  const scopeOptions = useMemo(() => {
    const options = scopeAvailability(context);
    return {
      ...options,
      // 检查按目录遍历，临时区不是图库的一部分，后端会直接拒绝。
      temp: { enabled: false, reason: "临时文件夹不参与检查" },
    };
  }, [context]);

  // 有没有值得回收的段：低于触发阈值的段压了也是白写一遍，按钮因此保持不可点。
  const reclaimable =
    packStats?.segments.some((segment) => segmentGarbageRatio(segment) > packStats.triggerRatio) ??
    false;

  // 勾了「整个图库」时下面的范围选择被禁用，不该再拿它的可用性挡按钮。
  const checkScopeReady =
    mode === "full" && fullLibrary ? true : scopeOptions[checkScope].enabled;

  const exportScopeOptions = useMemo(() => scopeAvailability(context), [context]);

  const launch = async (request: BatchTaskRequest, onStarted: (id: string) => void) => {
    setStarting(true);
    setError(null);
    try {
      const result = await start(request);
      if (result.status === "conflict") {
        const active = result.active.map((task) => task.label).join("、");
        setError(`有任务正在运行：${active || "未知任务"}`);
        return;
      }
      onStarted(result.task.taskId);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setStarting(false);
    }
  };

  const startCheck = async () => {
    if (!context) return;
    const scope: TaskScope | null =
      mode === "full" && fullLibrary ? { type: "library" } : buildScope(checkScope, context);
    if (!scope) return;
    if (mode === "full" && fullLibrary) {
      const accepted = await confirm({
        title: "对整个图库做完整校验？",
        message:
          "完整校验要逐张重算 BLAKE3，全库几百 GB 时通常要二十分钟以上，机械硬盘可能超过一小时，期间磁盘会一直读。可以随时取消，已扫到的结果会保留。",
        confirmLabel: "开始校验",
      });
      if (!accepted) return;
    }
    setReport(null);
    loadedReportFor.current = null;
    await launch(
      {
        kind: mode === "fast" ? "checkFast" : "checkFull",
        scope,
        overwrite: false,
        recalcVisuals: false,
        prompt: null,
        maxTokens: null,
        conflict: "queue",
      },
      setCheckTaskId,
    );
  };

  const startOrphanMove = async () => {
    if (!report || selectedOrphans.size === 0) return;
    setOrphanBusy(true);
    try {
      const result = await maintenanceApi.moveOrphans(report.taskId, [...selectedOrphans]);
      if (result.status === "conflict") {
        setError("有任务正在运行，等它结束后再试");
        return;
      }
      setOrphanTaskId(result.task.taskId);
      const count = selectedOrphans.size;
      setSelectedOrphans(new Set());
      notify(`正在移入回收站：${count.toLocaleString()} 个文件`, "info");
    } catch (cause) {
      notify("移入回收站失败", "error", errorMessage(cause));
    } finally {
      setOrphanBusy(false);
    }
  };

  const startPackCompact = async () => {
    setError(null);
    try {
      const result = await maintenanceApi.compactNow();
      if (result.status === "conflict") {
        setError("有任务正在运行，等它结束后再回收");
        return;
      }
      setPackTaskId(result.task.taskId);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const startExport = async () => {
    if (!context) return;
    const scope = buildScope(exportScope, context);
    if (!scope) return;
    if (!exportDir) {
      setError("请先选择导出目录");
      return;
    }
    await launch(
      {
        kind: "export",
        scope,
        overwrite: false,
        recalcVisuals: false,
        prompt: null,
        maxTokens: null,
        targetDir: exportDir,
        includeManifest,
        conflict: "queue",
      },
      setExportTaskId,
    );
  };

  const saveReport = async (group?: ReportGroup) => {
    if (!report) return;
    const suffix = group ? `-${GROUP_LABELS[group]}` : "";
    try {
      const path = await saveDialog({
        defaultPath: `检查报告${suffix}-${new Date(report.finishedAt).toISOString().slice(0, 10)}.csv`,
        filters: [{ name: "CSV", extensions: ["csv"] }],
      });
      if (!path) return;
      await maintenanceApi.saveInspectionReport(report.taskId, path, group);
      notify("报告已导出", "success", path);
    } catch (cause) {
      notify("导出报告失败", "error", errorMessage(cause));
    }
  };

  const pickExportDir = async () => {
    try {
      const picked = await openDialog({ directory: true, multiple: false });
      if (typeof picked === "string") setExportDir(picked);
    } catch (cause) {
      notify("选择目录失败", "error", errorMessage(cause));
    }
  };

  const cancelCheck = () => {
    if (checkTask) void cancel(checkTask.taskId);
  };

  const cancelPackCompact = () => {
    if (packTask) void cancel(packTask.taskId);
  };

  const cancelExport = () => {
    if (exportTask) void cancel(exportTask.taskId);
  };

  const toggleAllOrphans = () => {
    if (!report) return;
    setSelectedOrphans(
      selectedOrphans.size === report.orphans.length
        ? new Set()
        : new Set(report.orphans.map((orphan) => orphan.path)),
    );
  };

  const toggleOrphan = (path: string) => {
    setSelectedOrphans((previous) => {
      const next = new Set(previous);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  const footer = (
    <Button variant="primary" onClick={onClose}>
      关闭
    </Button>
  );

  return (
    <Modal
      open={open}
      title="维护与检查"
      subtitle="检查原图、处理孤儿、回收缩略图空间、导出图片。"
      width={860}
      onClose={onClose}
      footer={footer}
    >
      <div className="maintenance">
        <div className="maintenance-tabs" role="tablist" aria-label="维护分类">
          {SECTIONS.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={section === id}
              className={`maintenance-tab${section === id ? " is-active" : ""}`}
              onClick={() => setSection(id)}
            >
              <Icon size={14} />
              {label}
            </button>
          ))}
        </div>

        {error ? <p className="dialog-empty dialog-error">{error}</p> : null}

        {section === "check" ? (
          <MaintenanceCheckSection
            mode={mode}
            onModeChange={setMode}
            checkScope={checkScope}
            onCheckScopeChange={setCheckScope}
            fullLibrary={fullLibrary}
            onFullLibraryChange={setFullLibrary}
            context={context}
            scopeOptions={scopeOptions}
            checkScopeReady={checkScopeReady}
            starting={starting}
            checkActive={checkActive}
            checkTask={checkTask}
            onStartCheck={() => void startCheck()}
            onCancelCheck={cancelCheck}
            report={report}
            onSaveReport={(group) => void saveReport(group)}
          />
        ) : null}

        {section === "orphans" ? (
          <MaintenanceOrphansSection
            report={report}
            selectedOrphans={selectedOrphans}
            onToggleAll={toggleAllOrphans}
            onToggleOrphan={toggleOrphan}
            orphanBusy={orphanBusy}
            orphanActive={orphanActive}
            orphanTask={orphanTask}
            onStartOrphanMove={() => void startOrphanMove()}
          />
        ) : null}

        {section === "packs" ? (
          <MaintenancePacksSection
            packStats={packStats}
            packStatus={packStatus}
            packError={packError}
            reclaimable={reclaimable}
            packActive={packActive}
            packTask={packTask}
            onStartPackCompact={() => void startPackCompact()}
            onCancelPackCompact={cancelPackCompact}
          />
        ) : null}

        {section === "export" ? (
          <MaintenanceExportSection
            context={context}
            exportScope={exportScope}
            onExportScopeChange={setExportScope}
            exportScopeOptions={exportScopeOptions}
            exportDir={exportDir}
            onPickExportDir={() => void pickExportDir()}
            includeManifest={includeManifest}
            onIncludeManifestChange={setIncludeManifest}
            starting={starting}
            exportActive={exportActive}
            exportTask={exportTask}
            onStartExport={() => void startExport()}
            onCancelExport={cancelExport}
          />
        ) : null}
      </div>
    </Modal>
  );
}

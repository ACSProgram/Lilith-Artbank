import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import {
  AlertTriangle,
  ClipboardCheck,
  Download,
  FileWarning,
  Package,
  Play,
  Square,
  Trash2,
} from "lucide-react";
import { Button } from "../shared/components/Button";
import { Modal } from "../shared/components/Modal";
import { Spinner } from "../shared/components/Spinner";
import { ScopePicker } from "../shared/components/ScopePicker";
import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import { errorMessage } from "../shared/errors";
import { formatBytes } from "../shared/format";
import {
  buildScope,
  defaultScopeChoice,
  scopeAvailability,
  type ScopeChoice,
  type TaskScope,
} from "../shared/scope";
import { useNotices } from "./noticeContext";
import { maintenanceApi } from "./maintenanceApi";
import {
  formatRelative,
  reportIsClean,
  REPORT_ITEM_LIMIT,
  segmentGarbageRatio,
  type InspectionReport,
  type PackStats,
  type ReportGroup,
} from "./maintenanceTypes";
import { useTaskCenter } from "./taskContext";
import { taskApi } from "./taskApi";
import {
  isTaskActive,
  taskPercent,
  TASK_PHASE_LABELS,
  type BatchTaskRequest,
  type TaskSnapshot,
} from "./taskTypes";
import { CancelPendingNote, TaskProgressBar } from "./taskProgress";

type Section = "check" | "orphans" | "packs" | "export";

const SECTIONS: { id: Section; label: string; icon: typeof Package }[] = [
  { id: "check", label: "检查", icon: ClipboardCheck },
  { id: "orphans", label: "孤儿", icon: FileWarning },
  { id: "packs", label: "包", icon: Package },
  { id: "export", label: "导出", icon: Download },
];

/** 报告里每张表最多渲染这么多行；再多的部分请导出 CSV。 */
const REPORT_ROWS = 200;

/** 分组名与 Rust `report_csv_group` 的分组键一致，同时用于导出文件名后缀。 */
const GROUP_LABELS: Record<ReportGroup, string> = {
  missing: "缺失原图",
  orphan: "孤儿文件",
  sizeMismatch: "大小不符",
  hashMismatch: "哈希不符",
};

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
          <>
            <section className="dialog-section">
              <h3>检查方式</h3>
              <div className="maintenance-choices">
                <button
                  type="button"
                  className={`maintenance-choice${mode === "fast" ? " is-active" : ""}`}
                  onClick={() => setMode("fast")}
                >
                  <strong>快速检查</strong>
                  <small>只看文件在不在、大小对不对。几十万张也就一两分钟。</small>
                </button>
                <button
                  type="button"
                  className={`maintenance-choice${mode === "full" ? " is-active" : ""}`}
                  onClick={() => setMode("full")}
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
                  onChange={setCheckScope}
                  context={context}
                  availability={scopeOptions}
                />
              </fieldset>
              {mode === "full" ? (
                <label className="toggle-row">
                  <input
                    type="checkbox"
                    checked={fullLibrary}
                    onChange={(event) => setFullLibrary(event.target.checked)}
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
                  onClick={() => void startCheck()}
                >
                  开始检查
                </Button>
                {checkActive ? (
                  <Button
                    variant="danger"
                    icon={<Square size={13} />}
                    onClick={() => void cancel(checkTask!.taskId)}
                  >
                    取消检查
                  </Button>
                ) : null}
              </div>
              {checkTask ? <TaskProgress task={checkTask} /> : null}
            </section>

            {report ? (
              <ReportView report={report} onSave={(group) => void saveReport(group)} />
            ) : (
              <p className="dialog-empty">还没有检查结果。运行一次检查后会在这里列出问题。</p>
            )}
          </>
        ) : null}

        {section === "orphans" ? (
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
                      onClick={() =>
                        setSelectedOrphans(
                          selectedOrphans.size === report.orphans.length
                            ? new Set()
                            : new Set(report.orphans.map((orphan) => orphan.path)),
                        )
                      }
                      disabled={report.orphans.length === 0}
                    >
                      {selectedOrphans.size === report.orphans.length ? "取消全选" : "全选"}
                    </Button>
                    <Button
                      variant="danger"
                      icon={<Trash2 size={13} />}
                      loading={orphanBusy}
                      disabled={selectedOrphans.size === 0 || orphanActive}
                      onClick={() => void startOrphanMove()}
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
                                onChange={() =>
                                  setSelectedOrphans((previous) => {
                                    const next = new Set(previous);
                                    if (next.has(orphan.path)) next.delete(orphan.path);
                                    else next.add(orphan.path);
                                    return next;
                                  })
                                }
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
        ) : null}

        {section === "packs" ? (
          <>
            <section className="dialog-section">
              <h3>
                <Package size={14} /> 缩略图段
              </h3>
              {packStatus === "loading" ? <Spinner size={14} /> : null}
              {packError ? <p className="dialog-empty dialog-error">{packError}</p> : null}
              {packStats ? (
                <>
                  <div className="maintenance-actions">
                    <Button
                      variant="secondary"
                      icon={<Play size={13} />}
                      disabled={packActive || !reclaimable}
                      title={
                        reclaimable
                          ? "现在就回收，不等空闲条件"
                          : "没有超过阈值的段，回收不会释放空间"
                      }
                      onClick={() => void startPackCompact()}
                    >
                      立即回收
                    </Button>
                    {packActive ? (
                      <Button
                        variant="danger"
                        icon={<Square size={13} />}
                        onClick={() => packTask && void cancel(packTask.taskId)}
                      >
                        取消
                      </Button>
                    ) : null}
                    <span className="maintenance-summary">
                      共 {formatBytes(packStats.totalSize)} · 已用{" "}
                      {formatBytes(packStats.totalUsed)} · 可回收{" "}
                      {formatBytes(packStats.garbageBytes)}
                    </span>
                  </div>
                  {packTask ? <TaskProgress task={packTask} /> : null}
                  <p className="dialog-hint">
                    {packStats.lastCompact
                      ? `上次回收：${formatRelative(packStats.lastCompact.at)} · 释放 ${formatBytes(
                          packStats.lastCompact.freedBytes,
                        )}`
                      : "还没有回收过。"}
                    {reclaimable
                      ? ` 垃圾率超过 ${(packStats.triggerRatio * 100).toFixed(0)}% 且空闲三分钟时自动回收。`
                      : " 现在没有需要回收的段。"}
                  </p>
                  <div className="maintenance-table-wrap">
                    <table className="maintenance-table">
                      <thead>
                        <tr>
                          <th>段</th>
                          <th>文件</th>
                          <th>大小</th>
                          <th>已用</th>
                          <th>垃圾</th>
                          <th>垃圾率</th>
                        </tr>
                      </thead>
                      <tbody>
                        {packStats.segments.map((segment) => (
                          <tr key={segment.id}>
                            <td className="list-numeric">#{segment.id}</td>
                            <td className="maintenance-path" title={segment.fileName}>
                              {segment.fileName}
                            </td>
                            <td className="list-numeric">{formatBytes(segment.size)}</td>
                            <td className="list-numeric">{formatBytes(segment.used)}</td>
                            <td className="list-numeric">{formatBytes(segment.garbage)}</td>
                            <td className="list-numeric">
                              {(segmentGarbageRatio(segment) * 100).toFixed(1)}%
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </>
              ) : null}
            </section>

            <section className="dialog-section">
              <h3>特征包</h3>
              <p className="dialog-hint">特征包只有统计，没有回收按钮：它们只在整库重建时才会重写。</p>
              {packStats && packStats.features.length > 0 ? (
                <div className="maintenance-table-wrap">
                  <table className="maintenance-table">
                    <thead>
                      <tr>
                        <th>类型</th>
                        <th>模型</th>
                        <th>维度</th>
                        <th>包</th>
                        <th>条数</th>
                        <th>已用</th>
                        <th>垃圾</th>
                      </tr>
                    </thead>
                    <tbody>
                      {packStats.features.map((pack) => (
                        <tr key={`${pack.kind}-${pack.model}-${pack.packName}`}>
                          <td>{pack.kind}</td>
                          <td className="maintenance-path" title={pack.model}>
                            {pack.model}
                          </td>
                          <td className="list-numeric">{pack.dim}</td>
                          <td className="maintenance-path" title={pack.packName}>
                            {pack.packName}
                          </td>
                          <td className="list-numeric">{pack.blobs.toLocaleString()}</td>
                          <td className="list-numeric">{formatBytes(pack.used)}</td>
                          <td className="list-numeric">{formatBytes(pack.garbage)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              ) : (
                <p className="dialog-empty">还没有生成过特征。</p>
              )}
            </section>
          </>
        ) : null}

        {section === "export" ? (
          <>
            <section className="dialog-section">
              <h3>范围</h3>
              <ScopePicker
                name="maintenance-export-scope"
                value={exportScope}
                onChange={setExportScope}
                context={context}
                availability={exportScopeOptions}
              />
            </section>
            <section className="dialog-section">
              <h3>目标目录</h3>
              <div className="maintenance-actions">
                <Button variant="secondary" onClick={() => void pickExportDir()}>
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
                  onChange={(event) => setIncludeManifest(event.target.checked)}
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
                  onClick={() => void startExport()}
                >
                  开始导出
                </Button>
                {exportActive ? (
                  <Button
                    variant="danger"
                    icon={<Square size={13} />}
                    onClick={() => exportTask && void cancel(exportTask.taskId)}
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
        ) : null}
      </div>
    </Modal>
  );
}

/** 任务进度条：与批量任务对话框同一套百分比口径。 */
function TaskProgress({ task, copiedBytes }: { task: TaskSnapshot; copiedBytes?: number }) {
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
function ReportView({
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

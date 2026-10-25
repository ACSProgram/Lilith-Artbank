import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Play, Square } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Field } from "../shared/components/Field";
import { Modal } from "../shared/components/Modal";
import { Spinner } from "../shared/components/Spinner";
import { formatBytes, formatDuration } from "../shared/format";
import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import { confirmOptimize } from "../shared/confirm";
import { errorMessage } from "../shared/errors";
import { useNotices } from "./noticeContext";
import { useTaskCenter } from "./taskContext";
import { taskApi } from "./taskApi";
import { OptimizeCurveChart, OptimizeShareChart, type OptimizeShareSegment } from "./OptimizeCharts";
import {
  TASK_PHASE_LABELS,
  isTaskActive,
  taskPercent,
  type BatchTaskRequest,
  type ConflictPolicy,
  type OptimizeEstimate,
  type OptimizeOptions,
  type OptimizeTier,
  type TaskSnapshot,
  type TaskSummary,
} from "./taskTypes";
import {
  buildScope,
  defaultScopeChoice,
  scopeAvailability,
  scopeCountLabel,
  type ScopeChoice,
} from "./taskScope";

interface OptimizeDialogProps {
  open: boolean;
  onClose: () => void;
  context: BatchTaskContextInfo | null;
}

const TIERS: Array<{ value: OptimizeTier; label: string; detail: string }> = [
  { value: "extreme", label: "极高", detail: "质量 95 起" },
  { value: "high", label: "高（默认）", detail: "质量 85 起" },
  { value: "medium", label: "中", detail: "质量 75 起" },
];

const OUTPUT_FORMATS: Array<{ value: OptimizeOptions["outputFormat"]; label: string }> = [
  { value: "jpeg", label: "JPEG" },
  { value: "webp", label: "WebP" },
];

const MAX_EDGES = [
  { value: null as number | null, label: "不限" },
  { value: 4000, label: "4000 px" },
  { value: 3000, label: "3000 px" },
  { value: 2400, label: "2400 px" },
];

const SAMPLE_SIZES = [100, 200, 500];

/** 范围的名字；数量另用 `scopeCountLabel`，两处展示要保持一致。 */
function scopeName(scope: ScopeChoice, context: BatchTaskContextInfo | null): string {
  switch (scope) {
    case "selected":
      return "当前选择";
    case "folder":
      return context?.folderName || "当前文件夹";
    case "collection":
      return context?.collectionName || "当前集合";
    case "view":
      return "当前视图";
    case "library":
      return "整个图库";
  }
}

/** 估算与执行共用同一块进度展示，只有数据来源不同。 */
function ProgressPanel({ task, percent }: { task: TaskSnapshot; percent: number }) {
  return (
    <div className="batch-task-progress">
      <div className="batch-task-progress-head">
        <strong>
          {task.label} · {TASK_PHASE_LABELS[task.phase]}
        </strong>
        <span>
          {(task.completed + task.failed + task.skipped).toLocaleString()} /{" "}
          {task.total.toLocaleString()}
        </span>
      </div>
      <div className="task-progress">
        <span style={{ width: `${percent}%` }} />
      </div>
      <div className="batch-task-progress-meta">
        <span>{task.message || (isTaskActive(task) ? <Spinner size={12} /> : "已结束")}</span>
      </div>
    </div>
  );
}

/** 跳过原因按体积从大到小展示；计数为 0 的不占版面。 */
function skipSegments(estimate: OptimizeEstimate): OptimizeShareSegment[] {
  return [
    { key: "compact", label: "已足够紧凑", ...estimate.skipCompact },
    { key: "optimized", label: "已处理过", ...estimate.skipOptimized },
    { key: "transparent", label: "有透明通道", ...estimate.skipTransparent },
    { key: "tooLarge", label: "超出 WebP 尺寸", ...estimate.skipTooLarge },
    { key: "color", label: "非 sRGB 色彩", ...estimate.skipColorProfile },
    { key: "quality", label: "画质不达标", ...estimate.skipQuality },
    { key: "notSmaller", label: "省得太少", ...estimate.skipNotSmaller },
    { key: "depth", label: "16 位源", ...estimate.skipDepth },
    { key: "failed", label: "读取失败", count: estimate.failedCount, bytes: estimate.failedBytes },
  ]
    .filter((segment) => segment.count > 0)
    .sort((left, right) => right.bytes - left.bytes);
}

export function OptimizeDialog({ open, onClose, context }: OptimizeDialogProps) {
  const { start, cancel, find } = useTaskCenter();
  const { confirm } = useNotices();
  const [scope, setScope] = useState<ScopeChoice>("library");
  const [tier, setTier] = useState<OptimizeTier>("high");
  const [outputFormat, setOutputFormat] =
    useState<OptimizeOptions["outputFormat"]>("jpeg");
  const [maxLongEdge, setMaxLongEdge] = useState<number | null>(null);
  const [skipOptimized, setSkipOptimized] = useState(true);
  const [regenerateThumbnails, setRegenerateThumbnails] = useState(false);
  const [recomputePhash, setRecomputePhash] = useState(false);
  const [recomputeColors, setRecomputeColors] = useState(false);
  const [sampleSize, setSampleSize] = useState(200);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [conflict, setConflict] = useState<TaskSummary[] | null>(null);
  const [taskId, setTaskId] = useState<string | null>(null);
  const [estimate, setEstimate] = useState<OptimizeEstimate | null>(null);
  const [loadingEstimate, setLoadingEstimate] = useState(false);
  const [executeTaskId, setExecuteTaskId] = useState<string | null>(null);
  const [pendingKind, setPendingKind] = useState<"estimate" | "execute">("estimate");
  // 结果在表单下方，估算完成时可能落在可视区外，需要主动滚进来。
  const resultRef = useRef<HTMLDivElement>(null);
  const fetchedEstimate = useRef<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setScope(defaultScopeChoice(context));
    setTier("high");
    setOutputFormat("jpeg");
    setMaxLongEdge(null);
    setSkipOptimized(true);
    setRegenerateThumbnails(false);
    setRecomputePhash(false);
    setRecomputeColors(false);
    setSampleSize(200);
    setStarting(false);
    setError(null);
    setConflict(null);
    setTaskId(null);
    setEstimate(null);
    setLoadingEstimate(false);
    setExecuteTaskId(null);
    setPendingKind("estimate");
    fetchedEstimate.current = null;
    // 只在打开时重置；任务进度不应打断用户正在改的选项。
  }, [open]);

  const options = useMemo<OptimizeOptions>(
    () => ({
      tier,
      outputFormat,
      maxLongEdge,
      skipOptimized,
      regenerateThumbnails,
      recomputePhash,
      recomputeColors,
    }),
    [
      tier,
      outputFormat,
      maxLongEdge,
      skipOptimized,
      regenerateThumbnails,
      recomputePhash,
      recomputeColors,
    ],
  );

  const signature = useMemo(
    () => JSON.stringify({ scope, sampleSize, options }),
    [scope, sampleSize, options],
  );

  useEffect(() => {
    setEstimate(null);
    setError(null);
  }, [signature]);

  const task = taskId ? find(taskId) : null;
  const execution = executeTaskId ? find(executeTaskId) : null;
  const activeTask = task && isTaskActive(task) ? task : null;
  const activeExecution = execution && isTaskActive(execution) ? execution : null;
  const percent = task ? taskPercent(task) : 0;
  const executionPercent = execution ? taskPercent(execution) : 0;

  useEffect(() => {
    if (!task || task.kind !== "optimizeEstimate" || task.phase !== "completed") return;
    if (fetchedEstimate.current === task.taskId) return;
    fetchedEstimate.current = task.taskId;
    let disposed = false;
    setLoadingEstimate(true);
    void taskApi
      .optimizeEstimate(task.taskId)
      .then((result) => {
        if (disposed) return;
        if (result) {
          setEstimate(result);
        } else {
          setError("估算结果已失效，请重新估算");
        }
      })
      .catch((cause: unknown) => {
        if (!disposed) setError(errorMessage(cause));
      })
      .finally(() => {
        if (!disposed) setLoadingEstimate(false);
      });
    return () => {
      disposed = true;
    };
  }, [task]);

  const scopeOptions = useMemo(() => scopeAvailability(context), [context]);

  useEffect(() => {
    if (estimate) resultRef.current?.scrollIntoView({ block: "start" });
  }, [estimate]);

  const launchTask = async (
    kind: "optimizeEstimate" | "optimize",
    policy: ConflictPolicy,
  ) => {
    if (!context) return;
    if (kind === "optimize" && !estimate) return;
    const builtScope = buildScope(scope, context);
    if (!builtScope) return;
    setStarting(true);
    setError(null);
    setConflict(null);
    setPendingKind(kind === "optimize" ? "execute" : "estimate");
    try {
      const request: BatchTaskRequest = {
        kind,
        scope: builtScope,
        overwrite: false,
        recalcVisuals: false,
        prompt: null,
        maxTokens: null,
        optimize: options,
        estimateId: kind === "optimize" ? (estimate?.taskId ?? null) : null,
        estimateSampleSize: kind === "optimizeEstimate" ? sampleSize : null,
        conflict: policy,
      };
      const result = await start(request);
      if (result.status === "conflict") {
        setConflict(result.active);
        return;
      }
      if (kind === "optimize") {
        setExecuteTaskId(result.task.taskId);
      } else {
        setTaskId(result.task.taskId);
        setEstimate(null);
      }
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setStarting(false);
    }
  };

  const confirmExecute = async () => {
    if (!estimate) return;
    const accepted = await confirmOptimize(
      confirm,
      estimate.processCount,
      estimate.savingsBytes,
    );
    if (accepted) await launchTask("optimize", "fail");
  };

  const retryKind = pendingKind === "execute" ? "optimize" : "optimizeEstimate";

  const footer = activeExecution ? (
    <>
      <Button variant="ghost" onClick={onClose}>
        隐藏
      </Button>
      <Button
        variant="danger"
        icon={<Square size={13} />}
        onClick={() => void cancel(activeExecution.taskId)}
      >
        取消任务
      </Button>
    </>
  ) : activeTask ? (
    <>
      <Button variant="ghost" onClick={onClose}>
        隐藏
      </Button>
      <Button
        variant="danger"
        icon={<Square size={13} />}
        onClick={() => void cancel(activeTask.taskId)}
      >
        取消任务
      </Button>
    </>
  ) : conflict ? (
    <>
      <Button variant="ghost" onClick={() => setConflict(null)}>
        返回
      </Button>
      <Button
        variant="secondary"
        onClick={() => void launchTask(retryKind, "cancelActive")}
        loading={starting}
      >
        取消现有任务并开始
      </Button>
      <Button
        variant="primary"
        onClick={() => void launchTask(retryKind, "queue")}
        loading={starting}
      >
        排队等待
      </Button>
    </>
  ) : execution ? (
    <Button variant="primary" onClick={onClose}>
      关闭
    </Button>
  ) : estimate ? (
    <>
      <Button
        variant="ghost"
        onClick={() => {
          setEstimate(null);
          setError(null);
        }}
      >
        返回修改
      </Button>
      <Button
        variant="danger"
        icon={<Play size={13} />}
        loading={starting}
        disabled={estimate.processCount === 0}
        onClick={() => void confirmExecute()}
      >
        确认执行
      </Button>
    </>
  ) : (
    <>
      <Button variant="ghost" onClick={onClose}>
        关闭
      </Button>
      <Button
        variant="primary"
        icon={<Play size={13} />}
        loading={starting || loadingEstimate}
        disabled={!scopeOptions[scope].enabled}
        onClick={() => void launchTask("optimizeEstimate", "fail")}
      >
        开始估算
      </Button>
    </>
  );

  const skippedCount = estimate
    ? Math.max(0, estimate.totalCount - estimate.processCount)
    : 0;
  const breakdown = estimate ? skipSegments(estimate) : [];
  const qualityTotal = estimate
    ? estimate.qualityCounts.reduce((sum, entry) => sum + entry.count, 0)
    : 0;

  return (
    <Modal
      open={open}
      title="优化图片空间"
      subtitle="先估算能省多少；确认执行后才会替换原图。"
      width={820}
      onClose={onClose}
      footer={footer}
    >
      <div className="batch-task">
        {!estimate && !execution ? (
          <>
            <Field label="处理范围">
              <div className="batch-task-scopes">
                {(Object.keys(scopeOptions) as ScopeChoice[]).map((value) => {
                  const availability = scopeOptions[value];
                  return (
                    <label
                      key={value}
                      className={`batch-task-scope${availability.enabled ? "" : " is-disabled"}`}
                      title={availability.reason}
                    >
                      <input
                        type="radio"
                        name="optimize-scope"
                        checked={scope === value}
                        disabled={!availability.enabled}
                        onChange={() => setScope(value)}
                      />
                      <span>{scopeName(value, context)}</span>
                      <em>
                        {availability.enabled ? scopeCountLabel(value, context) : availability.reason}
                      </em>
                    </label>
                  );
                })}
              </div>
            </Field>

            <div className="optimize-section">
              <span className="field-label">质量档位</span>
              <div className="optimize-tiers">
                {TIERS.map((item) => (
                  <button
                    key={item.value}
                    type="button"
                    className={`batch-task-type${tier === item.value ? " is-active" : ""}`}
                    onClick={() => setTier(item.value)}
                  >
                    <span>{item.label}</span>
                    <small>{item.detail}</small>
                  </button>
                ))}
              </div>
              <p className="field-hint">只压“存得浪费”的图，压不动的自动跳过。</p>
            </div>

            <div className="settings-grid">
              <Field label="输出格式" hint="WebP 更省，但不保留 EXIF 与色彩配置；选 JPEG 时透明图会被跳过。">
                <div className="optimize-choice-row">
                  {OUTPUT_FORMATS.map((item) => (
                    <button
                      key={item.value}
                      type="button"
                      className={`optimize-choice${outputFormat === item.value ? " is-active" : ""}`}
                      onClick={() => setOutputFormat(item.value)}
                    >
                      {item.label}
                    </button>
                  ))}
                </div>
              </Field>
              <Field label="长边上限" hint="超过上限的图片会被缩小，像素永久丢失。">
                <select
                  className="input"
                  value={maxLongEdge ?? ""}
                  onChange={(event) =>
                    setMaxLongEdge(event.target.value === "" ? null : Number(event.target.value))
                  }
                >
                  {MAX_EDGES.map((item) => (
                    <option key={item.label} value={item.value ?? ""}>
                      {item.label}
                    </option>
                  ))}
                </select>
              </Field>
            </div>

            <div className="settings-grid">
              <div className="field">
                <span className="field-label">重新计算信息</span>
                <div className="optimize-checks">
                  <label className="optimize-check" title="按压缩后的原图重建 512 px 缩略图">
                    <input
                      type="checkbox"
                      checked={regenerateThumbnails}
                      onChange={(event) => setRegenerateThumbnails(event.target.checked)}
                    />
                    <span>缩略图</span>
                  </label>
                  <label className="optimize-check" title="不重算的话，找相似与相似分组仍用旧 pHash">
                    <input
                      type="checkbox"
                      checked={recomputePhash}
                      onChange={(event) => setRecomputePhash(event.target.checked)}
                    />
                    <span>pHash</span>
                  </label>
                  <label className="optimize-check" title="不重算的话，按颜色筛选仍用旧主色">
                    <input
                      type="checkbox"
                      checked={recomputeColors}
                      onChange={(event) => setRecomputeColors(event.target.checked)}
                    />
                    <span>主色</span>
                  </label>
                </div>
              </div>
              <Field label="估算抽样" hint="样本越多越准，也越慢。">
                <select
                  className="input"
                  value={sampleSize}
                  onChange={(event) => setSampleSize(Number(event.target.value))}
                >
                  {SAMPLE_SIZES.map((value) => (
                    <option key={value} value={value}>
                      {value} 张
                    </option>
                  ))}
                </select>
              </Field>
            </div>

            <label className="toggle-row">
              <input
                type="checkbox"
                checked={skipOptimized}
                onChange={(event) => setSkipOptimized(event.target.checked)}
              />
              <span>
                <strong>跳过已处理过的图片</strong>
                <small>取消勾选会把这些图片再压一次，画质会继续下降。</small>
              </span>
            </label>
          </>
        ) : null}

        {estimate ? (
          <div className="optimize-run-summary">
            <span className="field-label">本次设置</span>
            <div className="optimize-outcome-rows">
              <div className="optimize-outcome-row">
                <span>范围</span>
                <strong>
                  {scopeName(scope, context)}
                  {scopeCountLabel(scope, context)
                    ? ` · ${scopeCountLabel(scope, context)}`
                    : ""}
                </strong>
              </div>
              <div className="optimize-outcome-row">
                <span>档位</span>
                <strong>
                  {tier === "extreme" ? "极高" : tier === "medium" ? "中" : "高"}
                </strong>
              </div>
              <div className="optimize-outcome-row">
                <span>输出</span>
                <strong>
                  {outputFormat === "jpeg" ? "JPEG" : "WebP"} · 长边{" "}
                  {maxLongEdge ? `${maxLongEdge} px` : "不限"}
                </strong>
              </div>
              <div className="optimize-outcome-row">
                <span>已处理图片</span>
                <strong>{skipOptimized ? "跳过" : "再压一次"}</strong>
              </div>
            </div>
          </div>
        ) : null}

        <div className="optimize-warning">
          <AlertTriangle size={14} />
          <span>执行压缩不可撤销，原图会被压缩后的版本替换。</span>
        </div>

        {conflict ? (
          <div className="batch-task-conflict">
            <span>
              <AlertTriangle size={14} /> 现在有任务正在运行：
            </span>
            <ul>
              {conflict.map((item) => (
                <li key={item.taskId}>
                  {item.label} · {item.scope}
                </li>
              ))}
            </ul>
            <p>
              可以等它结束后自动开始，或者取消它、马上开始
              {pendingKind === "execute" ? "压缩" : "新的估算"}。
            </p>
          </div>
        ) : null}

        {task && (isTaskActive(task) || (!estimate && !execution)) ? (
          <ProgressPanel task={task} percent={percent} />
        ) : null}

        {execution ? <ProgressPanel task={execution} percent={executionPercent} /> : null}

        {loadingEstimate ? (
          <div className="optimize-loading">
            <Spinner size={14} /> 正在读取估算结果…
          </div>
        ) : null}

        {estimate ? (
          <div className="optimize-result" ref={resultRef}>
            <div className="optimize-result-head">
              <strong>估算完成</strong>
              <span>
                样本 {estimate.sampleCount.toLocaleString()} 张 · 覆盖{" "}
                {(estimate.coverage * 100).toFixed(1)}% 的候选体积
              </span>
            </div>

            <div className="optimize-summary">
              <OptimizeShareChart
                processed={{
                  key: "process",
                  label: "处理",
                  count: estimate.processCount,
                  bytes: estimate.processBytes,
                }}
                skipped={{
                  key: "skip",
                  label: "跳过",
                  count: skippedCount,
                  bytes: Math.max(0, estimate.totalBytes - estimate.processBytes),
                }}
                breakdown={breakdown}
              />

              <div className="optimize-outcome">
                <div className="optimize-outcome-main">
                  <span>预计节省</span>
                  <strong>{formatBytes(estimate.savingsBytes)}</strong>
                  {estimate.savingsBytes > 0 ? (
                    <em>
                      {estimate.savingsMarginBytes > 0
                        ? `± ${formatBytes(estimate.savingsMarginBytes)}`
                        : "误差可忽略"}
                    </em>
                  ) : (
                    <em>这个范围没有可压缩的图片</em>
                  )}
                </div>
                <div className="optimize-outcome-rows">
                  <div className="optimize-outcome-row">
                    <span>体积</span>
                    <strong>
                      {formatBytes(estimate.totalBytes)} → {formatBytes(estimate.predictedBytes)}
                    </strong>
                  </div>
                  <div className="optimize-outcome-row">
                    <span>预计耗时</span>
                    <strong>{formatDuration(estimate.estimatedSeconds)}</strong>
                  </div>
                  <div className="optimize-outcome-row">
                    <span>候选</span>
                    <strong>{estimate.candidateCount.toLocaleString()} 张</strong>
                  </div>
                </div>
              </div>
            </div>

            {estimate.curve.length > 1 && estimate.savingsBytes > 0 ? (
              <div className="optimize-curve">
                <span className="field-label">收益曲线</span>
                <OptimizeCurveChart
                  points={estimate.curve}
                  totalSavings={estimate.savingsBytes}
                />
              </div>
            ) : null}

            {qualityTotal > 0 ? (
              <div className="optimize-quality">
                <span className="field-label">质量分布</span>
                <div className="optimize-quality-chips">
                  {estimate.qualityCounts.map((entry) => (
                    <span key={entry.quality} className="optimize-quality-chip">
                      q{entry.quality}
                      <em>{Math.round((entry.count / qualityTotal) * 100)}%</em>
                    </span>
                  ))}
                </div>
              </div>
            ) : null}

            {estimate.folders.length > 0 ? (
              <details className="optimize-folders">
                <summary>按文件夹明细（{estimate.folders.length} 个）</summary>
                <div className="optimize-folder-list">
                  {estimate.folders.map((folder) => (
                    <div key={folder.folderId} className="optimize-folder-row">
                      <span title={folder.path}>{folder.path || "根目录"}</span>
                      <em>{folder.count.toLocaleString()} 张</em>
                      <em>{formatBytes(folder.bytes)}</em>
                      <strong>省 {formatBytes(folder.savingsBytes)}</strong>
                    </div>
                  ))}
                </div>
              </details>
            ) : null}

            {estimate.sampleFailures.length > 0 ? (
              <details className="optimize-folders">
                <summary>样本失败 {estimate.sampleFailures.length} 张</summary>
                <div className="optimize-folder-list">
                  {estimate.sampleFailures.map((failure) => (
                    <div key={failure} className="optimize-folder-row">
                      <span title={failure}>{failure}</span>
                    </div>
                  ))}
                </div>
              </details>
            ) : null}
          </div>
        ) : null}

        {error ? <div className="form-error">{error}</div> : null}
      </div>
    </Modal>
  );
}

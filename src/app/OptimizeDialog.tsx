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

/** 与 `imageops/optimize.rs` 的边界保持一致；留空表示不限。 */
const MIN_LONG_EDGE = 64;
const MAX_LONG_EDGE = 65_535;
const WEBP_MAX_EDGE = 16_383;
const EDGE_PRESETS = [4000, 3000, 2400];

const SAMPLE_MIN = 50;
const SAMPLE_MAX = 2000;
const SAMPLE_STEP = 50;
const SAMPLE_SETTLE_MS = 300;
/** 实测编码吞吐（张/秒，见归档的《图片空间优化计划》第 13 节），只用来给抽样等一个大概。 */
const SAMPLE_RATE_JPEG = 61;
const SAMPLE_RATE_WEBP = 12.7;

/** 空值表示不限；非整数或越界都算非法。 */
function parseEdge(text: string): { value: number | null; invalid: boolean } {
  const trimmed = text.trim();
  if (!trimmed) return { value: null, invalid: false };
  const value = Number(trimmed);
  if (!Number.isInteger(value) || value < MIN_LONG_EDGE || value > MAX_LONG_EDGE) {
    return { value: null, invalid: true };
  }
  return { value, invalid: false };
}

/** 抽样编码的粗略等待时间；只按输出格式与样本数折算。 */
function sampleSeconds(size: number, format: OptimizeOptions["outputFormat"]): number {
  const rate = format === "webp" ? SAMPLE_RATE_WEBP : SAMPLE_RATE_JPEG;
  return size / rate;
}

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
  // 估算统计候选时没有可按张计的进度，只留百分比与文案。
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
  const [maxLongEdgeText, setMaxLongEdgeText] = useState("");
  const [edgeTouched, setEdgeTouched] = useState(false);
  const [skipOptimized, setSkipOptimized] = useState(true);
  const [regenerateThumbnails, setRegenerateThumbnails] = useState(false);
  const [recomputePhash, setRecomputePhash] = useState(false);
  const [recomputeColors, setRecomputeColors] = useState(false);
  const [sampleSize, setSampleSize] = useState(200);
  const [samplePending, setSamplePending] = useState(false);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [conflict, setConflict] = useState<TaskSummary[] | null>(null);
  const [taskId, setTaskId] = useState<string | null>(null);
  const [estimate, setEstimate] = useState<OptimizeEstimate | null>(null);
  const [loadingEstimate, setLoadingEstimate] = useState(false);
  const [estimateRetry, setEstimateRetry] = useState(0);
  const [canRetryEstimate, setCanRetryEstimate] = useState(false);
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
    setMaxLongEdgeText("");
    setEdgeTouched(false);
    setSkipOptimized(true);
    setRegenerateThumbnails(false);
    setRecomputePhash(false);
    setRecomputeColors(false);
    setSampleSize(200);
    setSamplePending(false);
    setStarting(false);
    setError(null);
    setConflict(null);
    setTaskId(null);
    setEstimate(null);
    setLoadingEstimate(false);
    setEstimateRetry(0);
    setCanRetryEstimate(false);
    setExecuteTaskId(null);
    setPendingKind("estimate");
    fetchedEstimate.current = null;
    // 只在打开时重置；任务进度不应打断用户正在改的选项。
  }, [open]);

  const edge = useMemo(() => parseEdge(maxLongEdgeText), [maxLongEdgeText]);

  const options = useMemo<OptimizeOptions>(
    () => ({
      tier,
      outputFormat,
      maxLongEdge: edge.value,
      skipOptimized,
      regenerateThumbnails,
      recomputePhash,
      recomputeColors,
    }),
    [
      tier,
      outputFormat,
      edge.value,
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
    setCanRetryEstimate(false);
  }, [signature]);

  // 拖动滑块时先禁用估算，松手 300ms 后才允许发起；避免每拖一格就重算一次。
  useEffect(() => {
    if (!samplePending) return;
    const timer = window.setTimeout(() => setSamplePending(false), SAMPLE_SETTLE_MS);
    return () => window.clearTimeout(timer);
  }, [samplePending, sampleSize]);

  const task = taskId ? find(taskId) : null;
  const execution = executeTaskId ? find(executeTaskId) : null;
  const activeTask = task && isTaskActive(task) ? task : null;
  const activeExecution = execution && isTaskActive(execution) ? execution : null;
  const percent = task ? taskPercent(task) : 0;
  const executionPercent = execution ? taskPercent(execution) : 0;

  // 只依赖原始值：任务快照每 150ms 换一次对象，用对象做依赖会在请求在飞时
  // 触发 cleanup，把结果丢掉并永久停在「正在读取估算结果…」。
  const estimateTaskId = task?.taskId ?? null;
  const estimateTaskKind = task?.kind ?? null;
  const estimateTaskPhase = task?.phase ?? null;

  useEffect(() => {
    if (
      !estimateTaskId ||
      estimateTaskKind !== "optimizeEstimate" ||
      estimateTaskPhase !== "completed"
    ) {
      return;
    }
    const requestKey = `${estimateTaskId}#${estimateRetry}`;
    if (fetchedEstimate.current === requestKey) return;
    fetchedEstimate.current = requestKey;
    let disposed = false;
    let settled = false;
    setLoadingEstimate(true);
    setError(null);
    setCanRetryEstimate(false);
    const timer = window.setTimeout(() => {
      if (disposed || settled) return;
      settled = true;
      setLoadingEstimate(false);
      setCanRetryEstimate(true);
      setError("读取估算结果超时，请重试");
      // 超时后允许重试再次发起同一任务的结果读取。
      if (fetchedEstimate.current === requestKey) fetchedEstimate.current = null;
    }, 5000);
    void taskApi
      .optimizeEstimate(estimateTaskId)
      .then((result) => {
        if (disposed) return;
        settled = true;
        window.clearTimeout(timer);
        if (result) {
          setEstimate(result);
          setError(null);
          setCanRetryEstimate(false);
        } else {
          setError("估算结果已被新的估算覆盖，请重新估算");
        }
      })
      .catch((cause: unknown) => {
        if (disposed) return;
        settled = true;
        window.clearTimeout(timer);
        setCanRetryEstimate(true);
        setError(errorMessage(cause));
      })
      .finally(() => {
        if (!disposed) setLoadingEstimate(false);
      });
    return () => {
      disposed = true;
      window.clearTimeout(timer);
    };
  }, [estimateTaskId, estimateTaskKind, estimateTaskPhase, estimateRetry]);

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
    if (edge.invalid) {
      setEdgeTouched(true);
      return;
    }
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
        setEstimateRetry(0);
        setCanRetryEstimate(false);
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
        disabled={!scopeOptions[scope].enabled || edge.invalid || samplePending}
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
                <div className="input-row">
                  <input
                    className={`input${edge.invalid && edgeTouched ? " is-invalid" : ""}`}
                    type="number"
                    inputMode="numeric"
                    min={MIN_LONG_EDGE}
                    max={MAX_LONG_EDGE}
                    placeholder="不限"
                    value={maxLongEdgeText}
                    onChange={(event) => setMaxLongEdgeText(event.target.value)}
                    onBlur={() => setEdgeTouched(true)}
                  />
                  <span className="input-unit">px</span>
                </div>
                <div className="optimize-choice-row">
                  {EDGE_PRESETS.map((value) => (
                    <button
                      key={value}
                      type="button"
                      className={`optimize-choice${edge.value === value ? " is-active" : ""}`}
                      onClick={() => {
                        setMaxLongEdgeText(String(value));
                        setEdgeTouched(true);
                      }}
                    >
                      {value}
                    </button>
                  ))}
                  <button
                    type="button"
                    className={`optimize-choice${edge.value === null && !edge.invalid ? " is-active" : ""}`}
                    onClick={() => {
                      setMaxLongEdgeText("");
                      setEdgeTouched(true);
                    }}
                  >
                    不限
                  </button>
                </div>
                {edge.invalid && edgeTouched ? (
                  <span className="field-error">长边上限必须是 64–65535 之间的整数</span>
                ) : null}
                {outputFormat === "webp" && edge.value !== null && edge.value > WEBP_MAX_EDGE ? (
                  <span className="field-hint">
                    超过 16383 px 的图无法编成 WebP，会被跳过。设为 16383 或更小可让它们先缩放再编码。
                  </span>
                ) : null}
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
                <div className="optimize-slider-head">
                  <strong>{sampleSize} 张</strong>
                  <span>约 {formatDuration(sampleSeconds(sampleSize, outputFormat))}</span>
                </div>
                <input
                  className="range"
                  type="range"
                  min={SAMPLE_MIN}
                  max={SAMPLE_MAX}
                  step={SAMPLE_STEP}
                  value={sampleSize}
                  onChange={(event) => {
                    setSampleSize(Number(event.target.value));
                    setSamplePending(true);
                  }}
                />
                <div className="optimize-slider-marks">
                  <span>50（快）</span>
                  <span>200（默认）</span>
                  <span>500</span>
                  <span>1000</span>
                  <span>2000（准）</span>
                </div>
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
                  {edge.value ? `${edge.value} px` : "不限"}
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

        {error ? (
          <div className="optimize-error-row">
            <div className="form-error">{error}</div>
            {canRetryEstimate ? (
              <Button
                size="sm"
                variant="secondary"
                onClick={() => setEstimateRetry((value) => value + 1)}
              >
                重试读取
              </Button>
            ) : null}
          </div>
        ) : null}
      </div>
    </Modal>
  );
}

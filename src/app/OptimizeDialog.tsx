import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Play, RotateCcw, Square } from "lucide-react";
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
  type TaskScope,
  type TaskSnapshot,
  type TaskSummary,
} from "./taskTypes";
import { ScopePicker } from "../shared/components/ScopePicker";
import {
  buildScope,
  defaultScopeChoice,
  scopeAvailability,
  scopeCountLabel,
  scopeName,
  type ScopeChoice,
} from "../shared/scope";

interface OptimizeDialogProps {
  open: boolean;
  onClose: () => void;
  context: BatchTaskContextInfo | null;
  /** 当前图库标识；换库后不接管上一个图库的任务。 */
  libraryId: string;
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

/** 抽样只有五档，滑块按索引走，标签与停靠点一一对应。 */
const SAMPLE_CHOICES: ReadonlyArray<{ size: number; label: string }> = [
  { size: 50, label: "50（快）" },
  { size: 200, label: "200（默认）" },
  { size: 500, label: "500" },
  { size: 1_000, label: "1000" },
  { size: 2_000, label: "2000（准）" },
];
const DEFAULT_SAMPLE_INDEX = 1;

/** 一次已完成的估算，用来给同一组设置提示抽样等待时间。 */
interface SampleMeasurement {
  elapsedMs: number;
  count: number;
}

interface RunConfig {
  scopeLabel: string;
  scope: TaskScope;
  options: OptimizeOptions;
  sampleSize: number;
  /** 发起时的设置签名；结果回来时用它判断设置是否已改。 */
  signature: string;
}

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

function sampleKey(format: OptimizeOptions["outputFormat"], edge: number | null, size: number) {
  return `${format}|${edge ?? "none"}|${size}`;
}

/** 抽样提示只描述“抽样编码要等多久”，与结果页的总预计耗时是两件事。 */
function sampleWaitLabel(
  measurement: SampleMeasurement | undefined,
  size: number,
): string | null {
  if (!measurement || measurement.count <= 0 || measurement.elapsedMs <= 0) return null;
  // 候选不足时实测样本少于档位，按实测单张耗时折算到当前档位。
  const seconds = (measurement.elapsedMs / 1000) * (size / measurement.count);
  if (seconds < 1) return "不到 1 秒";
  return formatDuration(seconds);
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

/** 前置排除：只靠数据库列就能判定，不进候选池。 */
function prefilterSegments(estimate: OptimizeEstimate): OptimizeShareSegment[] {
  return [
    { key: "compact", label: "已足够紧凑", ...estimate.skipCompact },
    { key: "optimized", label: "已处理过", ...estimate.skipOptimized },
    { key: "transparent", label: "有透明通道", ...estimate.skipTransparent },
    { key: "tooLarge", label: "超出 WebP 尺寸", ...estimate.skipTooLarge },
    { key: "animated", label: "GIF", ...estimate.skipAnimated },
  ]
    .filter((segment) => segment.count > 0)
    .sort((left, right) => right.bytes - left.bytes);
}

/** 候选内跳过：解码后才发现不处理。 */
function candidateSkipSegments(estimate: OptimizeEstimate): OptimizeShareSegment[] {
  return [
    { key: "color", label: "非 sRGB 色彩", ...estimate.skipColorProfile },
    { key: "depth", label: "16 位源", ...estimate.skipDepth },
    { key: "quality", label: "画质不达标", ...estimate.skipQuality },
    { key: "notSmaller", label: "省得太少", ...estimate.skipNotSmaller },
    { key: "multiframe", label: "动画/多页", ...estimate.skipMultiframe },
    { key: "late", label: "解码后判定", ...estimate.skipLate },
    { key: "failed", label: "读取失败", count: estimate.failedCount, bytes: estimate.failedBytes },
  ]
    .filter((segment) => segment.count > 0)
    .sort((left, right) => right.bytes - left.bytes);
}

function scopeChoiceOf(scope: TaskScope | null | undefined): ScopeChoice | null {
  if (!scope) return null;
  switch (scope.type) {
    case "selected":
      return "selected";
    case "folder":
      return "folder";
    case "collection":
      return "collection";
    case "view":
      return "view";
    case "temp":
      return "temp";
    case "library":
      return "library";
    default:
      return null;
  }
}

export function OptimizeDialog({ open, onClose, context, libraryId }: OptimizeDialogProps) {
  const { tasks, start, cancel, find } = useTaskCenter();
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
  const [sampleSize, setSampleSize] = useState<number>(
    SAMPLE_CHOICES[DEFAULT_SAMPLE_INDEX].size,
  );
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
  const [runConfig, setRunConfig] = useState<RunConfig | null>(null);
  /** 当前展示的结果对应的设置签名；与表单签名不一致时结果作废。 */
  const [resultSignature, setResultSignature] = useState<string | null>(null);
  const [measurements, setMeasurements] = useState<Record<string, SampleMeasurement>>({});
  // 结果在表单下方，估算完成时可能落在可视区外，需要主动滚进来。
  const resultRef = useRef<HTMLDivElement>(null);
  const fetchedEstimate = useRef<string | null>(null);
  /** 已放弃的结果不再自动弹回。 */
  const abandoned = useRef<Set<string>>(new Set());
  /** 本会话发起过的估算任务；只有它们才做“设置已改”的校验。 */
  const launched = useRef<Set<string>>(new Set());
  const signatureRef = useRef("");
  const tasksRef = useRef(tasks);
  tasksRef.current = tasks;
  const runConfigRef = useRef<RunConfig | null>(null);
  runConfigRef.current = runConfig;

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
  signatureRef.current = signature;

  // 换库时丢掉上一个图库的会话状态，避免接管到别的库的任务。
  useEffect(() => {
    setRunConfig(null);
    setTaskId(null);
    setExecuteTaskId(null);
    setEstimate(null);
    abandoned.current.clear();
    launched.current.clear();
  }, [libraryId]);

  /** 打开窗口时按“执行 → 估算中 → 已有结果 → 已完成估算 → 默认配置”的顺序接管。 */
  useEffect(() => {
    if (!open) return;
    setStarting(false);
    setError(null);
    setConflict(null);
    setCanRetryEstimate(false);
    setEstimateRetry(0);
    setPendingKind("estimate");

    const list = tasksRef.current;
    const newest = (predicate: (task: TaskSnapshot) => boolean) =>
      list
        .filter(predicate)
        .sort((left, right) => right.startedAt - left.startedAt)
        .at(0) ?? null;

    const runningExecution = newest((task) => task.kind === "optimize" && isTaskActive(task));
    if (runningExecution) {
      setExecuteTaskId(runningExecution.taskId);
      return;
    }
    const runningEstimate = newest(
      (task) =>
        task.kind === "optimizeEstimate" &&
        isTaskActive(task) &&
        !abandoned.current.has(task.taskId),
    );
    if (runningEstimate) {
      setTaskId(runningEstimate.taskId);
      return;
    }
    if (estimate) return;

    // 本会话里已完成、且没被放弃的估算或执行：保留现场。
    if (taskId) {
      const previous = list.find((item) => item.taskId === taskId);
      if (previous && previous.phase === "completed" && !abandoned.current.has(taskId)) {
        return;
      }
    }
    if (executeTaskId) {
      const previous = list.find((item) => item.taskId === executeTaskId);
      if (previous && previous.phase === "completed") return;
    }

    // 默认配置。
    setScope(defaultScopeChoice(context));
    setTier("high");
    setOutputFormat("jpeg");
    setMaxLongEdgeText("");
    setEdgeTouched(false);
    setSkipOptimized(true);
    setRegenerateThumbnails(false);
    setRecomputePhash(false);
    setRecomputeColors(false);
    setSampleSize(SAMPLE_CHOICES[DEFAULT_SAMPLE_INDEX].size);
    setTaskId(null);
    setExecuteTaskId(null);
    setEstimate(null);
    setRunConfig(null);
    fetchedEstimate.current = null;
    // 只在打开时决策；任务进度不应打断用户正在改的选项。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // 设置一改，展示中的结果就作废；正在跑的旧任务完成时也会被签名校验挡住。
  useEffect(() => {
    if (!estimate || resultSignature === signature) return;
    abandoned.current.add(estimate.taskId);
    setEstimate(null);
    setResultSignature(null);
    setError(null);
    setCanRetryEstimate(false);
  }, [signature, estimate, resultSignature]);

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
        if (!result) {
          setError("估算结果已被新的估算覆盖，请重新估算");
          return;
        }
        if (result.libraryId && libraryId && result.libraryId !== libraryId) {
          setError("这条估算属于另一个图库，请重新估算");
          return;
        }
        // 设置在本会话里改过：旧任务的结果与当前参数不符，直接丢弃。
        if (
          launched.current.has(estimateTaskId) &&
          runConfigRef.current?.signature !== signatureRef.current
        ) {
          setError("估算运行期间设置已改变，请重新估算");
          return;
        }
        applyEstimate(result);
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
  }, [estimateTaskId, estimateTaskKind, estimateTaskPhase, estimateRetry, libraryId]);

  /** 用结果自身的参数回填表单与执行配置，接管时不依赖前端内存。 */
  function applyEstimate(result: OptimizeEstimate) {
    const restored = result.options ?? null;
    // 统一成表单里的键序与默认值，签名比对才不会因序列化差异误判。
    const normalized: OptimizeOptions = restored
      ? {
          tier: restored.tier,
          outputFormat: restored.outputFormat,
          maxLongEdge: restored.maxLongEdge ?? null,
          skipOptimized: restored.skipOptimized,
          regenerateThumbnails: restored.regenerateThumbnails,
          recomputePhash: restored.recomputePhash,
          recomputeColors: restored.recomputeColors,
        }
      : options;
    const choice = scopeChoiceOf(result.scope);
    if (choice) setScope(choice);
    if (restored) {
      setTier(normalized.tier);
      setOutputFormat(normalized.outputFormat);
      setMaxLongEdgeText(
        normalized.maxLongEdge != null ? String(normalized.maxLongEdge) : "",
      );
      setSkipOptimized(normalized.skipOptimized);
      setRegenerateThumbnails(normalized.regenerateThumbnails);
      setRecomputePhash(normalized.recomputePhash);
      setRecomputeColors(normalized.recomputeColors);
    }
    if (result.sampleSize > 0) setSampleSize(result.sampleSize);
    if (result.scope) {
      setRunConfig({
        scopeLabel: result.scopeLabel || "当前范围",
        scope: result.scope,
        options: normalized,
        sampleSize: result.sampleSize,
        signature: signatureRef.current,
      });
    }
    if (result.sampleElapsedMs > 0 && result.sampleCount > 0) {
      const key = sampleKey(normalized.outputFormat, normalized.maxLongEdge, result.sampleSize);
      setMeasurements((previous) => ({
        ...previous,
        [key]: { elapsedMs: result.sampleElapsedMs, count: result.sampleCount },
      }));
    }
    // 结果对应的签名按回填后的设置算，避免回填本身触发“设置已改”。
    const restoredSignature = JSON.stringify({
      scope: choice ?? scope,
      sampleSize: result.sampleSize > 0 ? result.sampleSize : sampleSize,
      options: normalized,
    });
    setResultSignature(restoredSignature);
    setEstimate(result);
    setError(null);
    setCanRetryEstimate(false);
  }

  const scopeOptions = useMemo(() => {
    const options = scopeAvailability(context);
    // 临时图片在关闭图库时整目录清空，压缩它们只会留下一条无用记录。
    options.temp = { enabled: false, reason: "临时图片不参与空间优化" };
    return options;
  }, [context]);

  useEffect(() => {
    if (estimate) resultRef.current?.scrollIntoView({ block: "start" });
  }, [estimate]);

  /** 按当前表单拼一份执行配置；范围不可用时返回 null。 */
  function buildFormConfig(target: BatchTaskContextInfo): RunConfig | null {
    const builtScope = buildScope(scope, target);
    if (!builtScope) return null;
    const countLabel = scopeCountLabel(scope, target);
    return {
      scopeLabel: `${scopeName(scope, target)}${countLabel ? ` · ${countLabel}` : ""}`,
      scope: builtScope,
      options,
      sampleSize,
      signature,
    };
  }

  const launchTask = async (
    kind: "optimizeEstimate" | "optimize",
    policy: ConflictPolicy,
  ) => {
    if (!context) return;
    if (kind === "optimize" && !estimate) return;
    if (kind === "optimizeEstimate" && edge.invalid) {
      setEdgeTouched(true);
      return;
    }
    // 执行优先用估算时记下的范围与参数，不用当前浏览状态重拼。
    const config = (kind === "optimize" ? runConfig : null) ?? buildFormConfig(context);
    if (!config) return;

    setStarting(true);
    setError(null);
    setConflict(null);
    setPendingKind(kind === "optimize" ? "execute" : "estimate");
    try {
      const request: BatchTaskRequest = {
        kind,
        scope: config.scope,
        overwrite: false,
        recalcVisuals: false,
        prompt: null,
        maxTokens: null,
        optimize: config.options,
        estimateId: kind === "optimize" ? (estimate?.taskId ?? null) : null,
        estimateSampleSize: kind === "optimizeEstimate" ? config.sampleSize : null,
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
        setRunConfig(config);
        setResultSignature(config.signature);
        launched.current.add(result.task.taskId);
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

  const resetToConfig = () => {
    setTaskId(null);
    setExecuteTaskId(null);
    setEstimate(null);
    setRunConfig(null);
    setError(null);
    setConflict(null);
    fetchedEstimate.current = null;
  };

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
    <>
      <Button variant="ghost" icon={<RotateCcw size={13} />} onClick={resetToConfig}>
        重新估算
      </Button>
      <Button variant="primary" onClick={onClose}>
        关闭
      </Button>
    </>
  ) : estimate ? (
    <>
      <Button
        variant="ghost"
        onClick={() => {
          // 记住这次结果已被放弃，下次打开不要自动弹回。
          abandoned.current.add(estimate.taskId);
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
        disabled={!scopeOptions[scope].enabled || edge.invalid}
        onClick={() => void launchTask("optimizeEstimate", "fail")}
      >
        开始估算
      </Button>
    </>
  );

  const candidateSkipCount = estimate
    ? Math.max(0, estimate.candidateCount - estimate.processCount)
    : 0;
  const candidateSkipBytes = estimate
    ? Math.max(0, estimate.candidateBytes - estimate.processBytes)
    : 0;
  const candidateBreakdown = estimate ? candidateSkipSegments(estimate) : [];
  const prefilterBreakdown = estimate ? prefilterSegments(estimate) : [];
  const qualityTotal = estimate
    ? estimate.qualityCounts.reduce((sum, entry) => sum + entry.count, 0)
    : 0;
  const sampleIndex = SAMPLE_CHOICES.findIndex((choice) => choice.size === sampleSize);
  const measurement = measurements[sampleKey(outputFormat, edge.value, sampleSize)];
  const waitLabel = sampleWaitLabel(measurement, sampleSize);
  const sampleShort = estimate ? estimate.sampleCount < estimate.sampleSize : false;
  const timingParts = estimate
    ? [
        { label: "读取/编码", value: estimate.estimatedWorkSeconds },
        { label: "写盘", value: estimate.estimatedWriteSeconds },
        { label: "提交/删除", value: estimate.estimatedFinishSeconds },
      ].filter((part) => part.value > 0)
    : [];

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
              <ScopePicker
                name="optimize-scope"
                value={scope}
                onChange={setScope}
                context={context}
                availability={scopeOptions}
              />
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
                  <strong>{sampleSize.toLocaleString()} 张</strong>
                  {waitLabel ? <span>约 {waitLabel}</span> : null}
                </div>
                <input
                  className="range"
                  type="range"
                  min={0}
                  max={SAMPLE_CHOICES.length - 1}
                  step={1}
                  value={sampleIndex < 0 ? DEFAULT_SAMPLE_INDEX : sampleIndex}
                  onChange={(event) =>
                    setSampleSize(SAMPLE_CHOICES[Number(event.target.value)].size)
                  }
                />
                <div className="optimize-slider-marks">
                  {SAMPLE_CHOICES.map((choice, index) => (
                    <span
                      key={choice.size}
                      style={{
                        left: `${(index / (SAMPLE_CHOICES.length - 1)) * 100}%`,
                      }}
                    >
                      {choice.label}
                    </span>
                  ))}
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

        {estimate && runConfig ? (
          <div className="optimize-run-summary">
            <span className="field-label">本次设置</span>
            <div className="optimize-outcome-rows">
              <div className="optimize-outcome-row">
                <span>范围</span>
                <strong>{runConfig.scopeLabel}</strong>
              </div>
              <div className="optimize-outcome-row">
                <span>档位</span>
                <strong>
                  {runConfig.options.tier === "extreme"
                    ? "极高"
                    : runConfig.options.tier === "medium"
                      ? "中"
                      : "高"}
                </strong>
              </div>
              <div className="optimize-outcome-row">
                <span>输出</span>
                <strong>
                  {runConfig.options.outputFormat === "jpeg" ? "JPEG" : "WebP"} · 长边{" "}
                  {runConfig.options.maxLongEdge
                    ? `${runConfig.options.maxLongEdge} px`
                    : "不限"}
                </strong>
              </div>
              <div className="optimize-outcome-row">
                <span>已处理图片</span>
                <strong>{runConfig.options.skipOptimized ? "跳过" : "再压一次"}</strong>
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
                样本 {estimate.sampleCount.toLocaleString()} 张
                {sampleShort ? "（候选不足）" : ""} · 覆盖{" "}
                {(estimate.coverage * 100).toFixed(1)}% 的候选体积
              </span>
            </div>

            <div className="optimize-funnel">
              <span>
                范围 {estimate.totalCount.toLocaleString()} 张 ·{" "}
                {formatBytes(estimate.totalBytes)}
              </span>
              <span>
                候选 {estimate.candidateCount.toLocaleString()} 张 ·{" "}
                {formatBytes(estimate.candidateBytes)}
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
                  label: "候选内跳过",
                  count: candidateSkipCount,
                  bytes: candidateSkipBytes,
                }}
                breakdown={candidateBreakdown}
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
                  {timingParts.length > 0 ? (
                    <div className="optimize-timing-detail">
                      其中：
                      {timingParts
                        .map((part) => `${part.label} ${formatDuration(part.value)}`)
                        .join(" · ")}
                    </div>
                  ) : null}
                </div>
              </div>
            </div>

            {prefilterBreakdown.length > 0 ? (
              <div className="optimize-prefilter">
                <span className="field-label">未纳入候选</span>
                <div className="optimize-prefilter-list">
                  {prefilterBreakdown.map((segment) => (
                    <div key={segment.key} className="optimize-prefilter-row">
                      <span>{segment.label}</span>
                      <em>{segment.count.toLocaleString()} 张</em>
                      <strong>{formatBytes(segment.bytes)}</strong>
                    </div>
                  ))}
                </div>
              </div>
            ) : null}

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

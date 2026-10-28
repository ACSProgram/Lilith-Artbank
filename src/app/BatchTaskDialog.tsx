import { useEffect, useMemo, useRef, useState } from "react";
import { Play, Square } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Field } from "../shared/components/Field";
import { Modal } from "../shared/components/Modal";
import { Spinner } from "../shared/components/Spinner";
import { DEFAULT_DESCRIPTION_PROMPT, type AiStatus } from "../modules/ai/types";
import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import { errorMessage } from "../shared/errors";
import { formatDuration, formatRate } from "../shared/format";
import {
  aiTaskAvailability,
  type ActionAvailability,
} from "../shared/capability";
import { useTaskCenter, useTaskData } from "./taskContext";
import { taskApi } from "./taskApi";
import {
  TASK_PHASE_LABELS,
  taskPercent,
  isTaskActive,
  type BatchTaskKind,
  type BatchTaskPreview,
  type BatchTaskRequest,
} from "./taskTypes";
import { ScopePicker } from "../shared/components/ScopePicker";
import {
  buildScope,
  defaultScopeChoice,
  scopeAvailability,
  scopeCountValue,
  type ScopeChoice,
} from "../shared/scope";
import {
  batchPreviewKey,
  canStartBatchTask,
  createBatchPreviewCache,
  previewSummary,
} from "./batchPreview";

interface BatchTaskDialogProps {
  open: boolean;
  onClose: () => void;
  context: BatchTaskContextInfo | null;
  status: AiStatus | null;
  onRefreshStatus: () => Promise<AiStatus | null>;
  /** 当前图库标识；换库后不显示上一个图库的预览。 */
  libraryId: string;
}

/**
 * 这个对话框只处理「范围 + 选项」这四类；检查、导出、包回收在维护页里，
 * 它们的参数形状不同（目标目录、报告 id），放进来只会让两边都别扭。
 */
type DialogKind = Extract<
  BatchTaskKind,
  "thumbnail" | "aiEmbedding" | "aiPose" | "aiDescription"
>;

const KIND_LABELS: Record<DialogKind, string> = {
  thumbnail: "重建缩略图",
  aiEmbedding: "Embedding",
  aiPose: "姿势",
  aiDescription: "描述",
};

const KIND_HINTS: Record<DialogKind, string> = {
  thumbnail: "为原图重新生成缩略图，可选重算 pHash 与主色。",
  aiEmbedding: "为图片生成语义向量，用于语义搜索与找相似。",
  aiPose: "识别画面里的人物姿势，用于按姿势找相似。",
  aiDescription: "为没有描述的图片生成文字描述。",
};

const AI_CAPABILITY: Partial<Record<DialogKind, string>> = {
  aiEmbedding: "imageEmbedding",
  aiPose: "pose",
  aiDescription: "description",
};

/** 预览查询在 20 万行上约百毫秒级；去抖只用来吸收连续切换。 */
const PREVIEW_DEBOUNCE_MS = 250;

export function BatchTaskDialog({
  open,
  onClose,
  context,
  status,
  onRefreshStatus,
  libraryId,
}: BatchTaskDialogProps) {
  const { start, cancel, find } = useTaskCenter();
  const { dataVersion } = useTaskData();
  const [kind, setKind] = useState<DialogKind>("thumbnail");
  const [scope, setScope] = useState<ScopeChoice>("library");
  const [overwrite, setOverwrite] = useState(false);
  const [recalcVisuals, setRecalcVisuals] = useState(false);
  const [prompt, setPrompt] = useState(DEFAULT_DESCRIPTION_PROMPT);
  const [maxTokens, setMaxTokens] = useState(160);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [taskId, setTaskId] = useState<string | null>(null);
  const [preview, setPreview] = useState<BatchTaskPreview | null>(null);
  const [previewStatus, setPreviewStatus] = useState<
    "idle" | "loading" | "ready" | "error"
  >("idle");
  const wasOpenRef = useRef(open);
  const previewCache = useRef(createBatchPreviewCache<BatchTaskPreview>()).current;
  const previewRequestKey = useRef<string | null>(null);

  const online = status?.online === true;

  useEffect(() => {
    if (!open) return;
    setKind("thumbnail");
    setScope(defaultScopeChoice(context));
    setOverwrite(false);
    setRecalcVisuals(false);
    setPrompt(DEFAULT_DESCRIPTION_PROMPT);
    setMaxTokens(160);
    setStarting(false);
    setError(null);
    setTaskId(null);
    // 只在打开时重置；任务状态变化不应打断用户输入。
  }, [open]);

  const descriptionConfigured = status?.health?.capabilities.description != null;

  const kindAvailability = useMemo(() => {
    const map = {} as Record<DialogKind, ActionAvailability>;
    for (const value of Object.keys(KIND_LABELS) as DialogKind[]) {
      if (value === "thumbnail") {
        map[value] = { enabled: true };
      } else if (value === "aiDescription" && online && !descriptionConfigured) {
        map[value] = { enabled: false, reason: "本机服务未配置描述模型" };
      } else {
        map[value] = aiTaskAvailability(online);
      }
    }
    return map;
  }, [online, descriptionConfigured]);

  const scopeOptions = useMemo(() => scopeAvailability(context), [context]);

  // 数据变化可能让当前范围失效（选择被清空、文件夹/集合消失）；只换范围，不动其它草稿。
  useEffect(() => {
    const wasOpen = wasOpenRef.current;
    wasOpenRef.current = open;
    if (!open || !context || !wasOpen) return;
    if (scopeOptions[scope]?.enabled) return;
    setScope(scopeOptions.view.enabled ? "view" : "library");
  }, [context, open, scope, scopeOptions]);

  const capability = AI_CAPABILITY[kind];
  const capabilityInfo = capability ? status?.health?.capabilities[capability] : undefined;
  const capabilityReady = capabilityInfo?.ready === true;

  const previewCapability = useMemo(() => {
    if (kind !== "aiEmbedding") return null;
    const model = capabilityInfo?.model?.trim();
    const dim = capabilityInfo?.dim ?? 0;
    return model && dim > 0 ? { model, dim } : null;
  }, [kind, capabilityInfo?.model, capabilityInfo?.dim]);

  const previewScope = useMemo(
    () => (context ? buildScope(scope, context) : null),
    [context, scope],
  );

  const previewKey = useMemo(() => {
    if (!open || !previewScope) return null;
    return batchPreviewKey({
      libraryId,
      kind,
      scope: previewScope,
      overwrite,
      capability: previewCapability,
      dataVersion,
    });
  }, [
    open,
    libraryId,
    kind,
    previewScope,
    overwrite,
    previewCapability,
    dataVersion,
  ]);

  // 键里已经带数据版本与图库，旧数字本来就不会命中；这里只是顺手释放内存。
  useEffect(() => {
    previewCache.clear();
  }, [libraryId, dataVersion, previewCache]);

  const launch = async () => {
    if (!context) return;
    const builtScope = buildScope(scope, context);
    if (!builtScope) return;
    setStarting(true);
    setError(null);
    try {
      const request: BatchTaskRequest = {
        kind,
        scope: builtScope,
        overwrite,
        recalcVisuals,
        prompt: kind === "aiDescription" ? prompt : null,
        maxTokens: kind === "aiDescription" ? maxTokens : null,
        conflict: "queue",
      };
      const result = await start(request);
      if (result.status === "conflict") {
        setError("任务暂时无法开始，请稍后再试");
        return;
      }
      setTaskId(result.task.taskId);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setStarting(false);
    }
  };

  const task = taskId ? find(taskId) : null;
  const activeTask = task && isTaskActive(task) ? task : null;
  const percent = task ? taskPercent(task) : 0;

  const previewOffline = kind !== "thumbnail" && !online;
  const previewDimMissing = kind === "aiEmbedding" && online && !previewCapability;

  useEffect(() => {
    if (!open || activeTask || !previewKey || !previewScope) {
      if (!open) {
        setPreview(null);
        setPreviewStatus("idle");
      }
      return;
    }
    if (previewOffline || previewDimMissing) {
      setPreview(null);
      setPreviewStatus("idle");
      return;
    }
    const cached = previewCache.get(previewKey);
    if (cached) {
      setPreview(cached);
      setPreviewStatus("ready");
      return;
    }
    setPreview(null);
    setPreviewStatus("loading");
    previewRequestKey.current = previewKey;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void taskApi
        .previewBatch({
          kind,
          scope: previewScope,
          overwrite,
          capability: kind === "aiEmbedding" ? previewCapability : null,
        })
        .then((result) => {
          if (cancelled || previewRequestKey.current !== previewKey) return;
          previewCache.set(previewKey, result);
          setPreview(result);
          setPreviewStatus("ready");
        })
        .catch(() => {
          if (cancelled || previewRequestKey.current !== previewKey) return;
          setPreviewStatus("error");
        });
    }, PREVIEW_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [
    open,
    activeTask,
    previewKey,
    previewScope,
    kind,
    overwrite,
    previewCapability,
    previewOffline,
    previewDimMissing,
    previewCache,
  ]);

  const fallbackTotal = scopeCountValue(scope, context);
  const previewLoading = previewStatus === "loading";
  const startDecision = canStartBatchTask({
    kindEnabled: kindAvailability[kind].enabled,
    scopeEnabled: scopeOptions[scope].enabled,
    embeddingDimMissing: previewDimMissing,
    previewLoading,
    willProcess: preview?.willProcess ?? null,
    previewFailed: previewStatus === "error",
  });

  const footer = activeTask ? (
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
  ) : (
    <>
      <Button variant="ghost" onClick={onClose}>
        取消
      </Button>
      <Button
        variant="primary"
        icon={<Play size={13} />}
        loading={starting}
        disabled={!startDecision.enabled}
        title={startDecision.reason ?? undefined}
        onClick={() => void launch()}
      >
        开始任务
      </Button>
    </>
  );

  return (
    <Modal
      open={open}
      title="批量任务"
      subtitle="任务在后台运行，关闭这个窗口后仍可在右下角查看进度。"
      width={720}
      onClose={onClose}
      footer={footer}
    >
      <div className="batch-task">
        <div className="batch-task-types" role="radiogroup" aria-label="任务类型">
          {(Object.keys(KIND_LABELS) as DialogKind[]).map((value) => {
            const availability = kindAvailability[value];
            return (
              <button
                key={value}
                type="button"
                role="radio"
                aria-checked={kind === value}
                className={`batch-task-type${kind === value ? " is-active" : ""}`}
                disabled={!availability.enabled}
                title={availability.reason}
                onClick={() => setKind(value)}
              >
                <span>{KIND_LABELS[value]}</span>
                <small>{KIND_HINTS[value]}</small>
              </button>
            );
          })}
        </div>

        <Field label="处理范围">
          <ScopePicker
            name="batch-scope"
            value={scope}
            onChange={setScope}
            context={context}
            availability={scopeOptions}
          />
        </Field>

        {context && !activeTask ? (
          <div className="batch-task-preview">
            <span>
              {previewSummary({
                total: preview?.total ?? fallbackTotal,
                willProcess: preview?.willProcess ?? null,
                loading: previewLoading,
                offline: previewOffline,
                dimMissing: previewDimMissing,
                failed: previewStatus === "error",
              })}
            </span>
            {previewLoading ? <Spinner size={12} /> : null}
          </div>
        ) : null}

        {kind !== "thumbnail" ? (
          <div className="batch-task-service">
            <span>
              {online ? "AI 服务在线" : "AI 服务离线"}
              {online && capabilityInfo
                ? ` · ${capabilityInfo.model || "模型名未知"}${
                    capabilityInfo.dim ? ` · ${capabilityInfo.dim} 维` : ""
                  }`
                : ""}
            </span>
            {online && capability ? (
              <em>{capabilityReady ? "模型已加载" : "开始任务时自动加载"}</em>
            ) : null}
          </div>
        ) : null}

        {kind === "thumbnail" ? (
          <div className="batch-task-options">
            <label className="toggle-row">
              <input
                type="checkbox"
                checked={overwrite}
                onChange={(event) => setOverwrite(event.target.checked)}
              />
              <span>
                <strong>重新生成已有缩略图</strong>
                <small>不勾选时只补齐缺失的缩略图。</small>
              </span>
            </label>
            <label className="toggle-row">
              <input
                type="checkbox"
                checked={recalcVisuals}
                onChange={(event) => setRecalcVisuals(event.target.checked)}
              />
              <span>
                <strong>同时重算 pHash 与主色</strong>
              </span>
            </label>
          </div>
        ) : null}

        {kind === "aiDescription" ? (
          <>
            <div className="batch-task-options">
              <label className="toggle-row">
                <input
                  type="checkbox"
                  checked={overwrite}
                  onChange={(event) => setOverwrite(event.target.checked)}
                />
                <span>
                  <strong>重新生成已有描述</strong>
                  <small>手写描述始终不会被覆盖。</small>
                </span>
              </label>
            </div>
            <div className="settings-grid">
              <Field label="提示词" hint="留空使用默认提示。">
                <textarea
                  className="input ai-prompt"
                  value={prompt}
                  onChange={(event) => setPrompt(event.target.value)}
                />
              </Field>
              <Field label="最大长度">
                <input
                  className="input"
                  type="number"
                  min={16}
                  max={512}
                  value={maxTokens}
                  onChange={(event) => setMaxTokens(Number(event.target.value))}
                />
              </Field>
            </div>
          </>
        ) : null}

        {kind === "aiEmbedding" || kind === "aiPose" ? (
          <div className="batch-task-options">
            <label className="toggle-row">
              <input
                type="checkbox"
                checked={overwrite}
                onChange={(event) => setOverwrite(event.target.checked)}
              />
              <span>
                <strong>重新生成已存在的特征</strong>
                <small>不勾选时只处理缺失项。</small>
              </span>
            </label>
          </div>
        ) : null}

        {task ? (
          <div className="batch-task-progress">
            <div className="batch-task-progress-head">
              <strong>
                {task.label} · {TASK_PHASE_LABELS[task.phase]}
              </strong>
              <span>
                {(task.completed + task.failed + task.skipped).toLocaleString()} /{" "}
                {task.total.toLocaleString()}
                {task.failed > 0 ? ` · 失败 ${task.failed}` : ""}
              </span>
            </div>
            <div className="task-progress">
              <span style={{ width: `${percent}%` }} />
            </div>
            <div className="batch-task-progress-meta">
              <span>
                {task.message ||
                  (isTaskActive(task) ? <Spinner size={12} /> : "已结束")}
              </span>
              {task.failed > 0 ? <span>失败 {task.failed.toLocaleString()} 张</span> : null}
            </div>
            {task.ratePerSecond != null ||
            task.etaSeconds != null ||
            task.currentFile ||
            task.currentImageId ? (
              <div className="batch-task-facts">
                {task.ratePerSecond != null ? (
                  <span>{formatRate(task.ratePerSecond)} 张/秒</span>
                ) : null}
                {task.etaSeconds != null ? (
                  <span>预计还需 {formatDuration(task.etaSeconds)}</span>
                ) : null}
                {task.currentFile ? (
                  <span className="batch-task-current" title={task.currentFile}>
                    当前：{task.currentFile}
                  </span>
                ) : task.currentImageId ? (
                  <span>当前：#{task.currentImageId}</span>
                ) : null}
              </div>
            ) : null}
          </div>
        ) : null}

        {error ? <div className="form-error">{error}</div> : null}
      </div>
    </Modal>
  );
}

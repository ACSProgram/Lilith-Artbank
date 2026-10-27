import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Play, Square } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Field } from "../shared/components/Field";
import { Modal } from "../shared/components/Modal";
import { Spinner } from "../shared/components/Spinner";
import { DEFAULT_DESCRIPTION_PROMPT, type AiStatus } from "../modules/ai/types";
import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import { errorMessage } from "../shared/errors";
import {
  aiTaskAvailability,
  type ActionAvailability,
} from "../shared/capability";
import { useTaskCenter } from "./taskContext";
import {
  TASK_PHASE_LABELS,
  taskPercent,
  isTaskActive,
  type BatchTaskKind,
  type BatchTaskRequest,
  type ConflictPolicy,
  type TaskSummary,
} from "./taskTypes";
import { ScopePicker } from "../shared/components/ScopePicker";
import {
  buildScope,
  defaultScopeChoice,
  scopeAvailability,
  type ScopeChoice,
} from "../shared/scope";

interface BatchTaskDialogProps {
  open: boolean;
  onClose: () => void;
  context: BatchTaskContextInfo | null;
  status: AiStatus | null;
  onRefreshStatus: () => Promise<AiStatus | null>;
}

const KIND_LABELS: Record<BatchTaskKind, string> = {
  thumbnail: "重建缩略图",
  aiEmbedding: "Embedding",
  aiPose: "姿势",
  aiDescription: "描述",
};

const KIND_HINTS: Record<BatchTaskKind, string> = {
  thumbnail: "为原图重新生成缩略图，可选重算 pHash 与主色。",
  aiEmbedding: "为图片生成语义向量，用于语义搜索与找相似。",
  aiPose: "识别画面里的人物姿势，用于按姿势找相似。",
  aiDescription: "为没有描述的图片生成文字描述。",
};

const AI_CAPABILITY: Partial<Record<BatchTaskKind, string>> = {
  aiEmbedding: "imageEmbedding",
  aiPose: "pose",
  aiDescription: "description",
};

export function BatchTaskDialog({
  open,
  onClose,
  context,
  status,
  onRefreshStatus,
}: BatchTaskDialogProps) {
  const { start, cancel, find } = useTaskCenter();
  const [kind, setKind] = useState<BatchTaskKind>("thumbnail");
  const [scope, setScope] = useState<ScopeChoice>("library");
  const [overwrite, setOverwrite] = useState(false);
  const [recalcVisuals, setRecalcVisuals] = useState(false);
  const [prompt, setPrompt] = useState(DEFAULT_DESCRIPTION_PROMPT);
  const [maxTokens, setMaxTokens] = useState(160);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [conflict, setConflict] = useState<TaskSummary[] | null>(null);
  const [taskId, setTaskId] = useState<string | null>(null);
  const wasOpenRef = useRef(open);

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
    setConflict(null);
    setTaskId(null);
    // 只在打开时重置；任务状态变化不应打断用户输入。
  }, [open]);

  const descriptionConfigured = status?.health?.capabilities.description != null;

  const kindAvailability = useMemo(() => {
    const map = {} as Record<BatchTaskKind, ActionAvailability>;
    for (const value of Object.keys(KIND_LABELS) as BatchTaskKind[]) {
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

  const launch = async (policy: ConflictPolicy) => {
    if (!context) return;
    const builtScope = buildScope(scope, context);
    if (!builtScope) return;
    setStarting(true);
    setError(null);
    setConflict(null);
    try {
      const request: BatchTaskRequest = {
        kind,
        scope: builtScope,
        overwrite,
        recalcVisuals,
        prompt: kind === "aiDescription" ? prompt : null,
        maxTokens: kind === "aiDescription" ? maxTokens : null,
        conflict: policy,
      };
      const result = await start(request);
      if (result.status === "conflict") {
        setConflict(result.active);
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
  ) : conflict ? (
    <>
      <Button variant="ghost" onClick={() => setConflict(null)}>
        返回
      </Button>
      <Button
        variant="secondary"
        onClick={() => void launch("cancelActive")}
        loading={starting}
      >
        取消现有任务并开始
      </Button>
      <Button variant="primary" onClick={() => void launch("queue")} loading={starting}>
        排队等待
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
        disabled={!kindAvailability[kind].enabled || !scopeOptions[scope].enabled}
        onClick={() => void launch("fail")}
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
          {(Object.keys(KIND_LABELS) as BatchTaskKind[]).map((value) => {
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

        {conflict ? (
          <div className="batch-task-conflict">
            <span>
              <AlertTriangle size={14} /> 现在有任务正在运行：
            </span>
            <ul>
              {conflict.map((taskItem) => (
                <li key={taskItem.taskId}>
                  {taskItem.label} · {taskItem.scope}
                </li>
              ))}
            </ul>
            <p>可以等它结束后自动开始，或者取消它、马上开始新任务。</p>
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
          </div>
        ) : null}

        {error ? <div className="form-error">{error}</div> : null}
      </div>
    </Modal>
  );
}

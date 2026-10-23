import { useEffect, useRef, useState } from "react";
import { Play, Square, RefreshCw, Sparkles, AlertTriangle, CheckCircle2 } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Field } from "../../shared/components/Field";
import { Modal } from "../../shared/components/Modal";
import { Spinner } from "../../shared/components/Spinner";
import { errorMessage } from "../../shared/errors";
import { aiApi, aiBrowserPreview } from "./api";
import {
  DEFAULT_DESCRIPTION_PROMPT,
  type AiJobKind,
  type AiJobPhase,
  type AiJobProgress,
  type AiJobSnapshot,
  type AiStatus,
} from "./types";

interface AiTaskDialogProps {
  open: boolean;
  onClose: () => void;
  selectedIds: number[];
  /** 图库查询条件；AI 模块不反向依赖图库类型，按不透明对象透传。 */
  query: object | null;
  status: AiStatus | null;
  onRefreshStatus: () => Promise<AiStatus | null>;
  onFinished: () => void;
}

type Scope = "selected" | "view" | "all";

const KIND_LABELS: Record<AiJobKind, string> = {
  embedding: "Embedding",
  pose: "姿势",
  description: "描述",
};

const PHASE_LABELS: Record<AiJobPhase, string> = {
  preparing: "准备中",
  running: "进行中",
  completed: "已完成",
  cancelled: "已取消",
  failed: "失败",
};

const CAPABILITY: Record<AiJobKind, string> = {
  embedding: "imageEmbedding",
  pose: "pose",
  description: "description",
};

export function AiTaskDialog({
  open,
  onClose,
  selectedIds,
  query,
  status,
  onRefreshStatus,
  onFinished,
}: AiTaskDialogProps) {
  const [kind, setKind] = useState<AiJobKind>("embedding");
  const [scope, setScope] = useState<Scope>("selected");
  const [overwrite, setOverwrite] = useState(false);
  const [prompt, setPrompt] = useState(DEFAULT_DESCRIPTION_PROMPT);
  const [maxTokens, setMaxTokens] = useState(160);
  const [job, setJob] = useState<AiJobSnapshot | null>(null);
  const [starting, setStarting] = useState(false);
  const [ensuring, setEnsuring] = useState(false);
  const [serviceBusy, setServiceBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const jobRef = useRef<AiJobSnapshot | null>(null);
  jobRef.current = job;
  const statusRef = useRef(status);
  statusRef.current = status;
  const finishedRef = useRef(false);

  const running = job?.phase === "preparing" || job?.phase === "running";

  useEffect(() => {
    if (!open) return;
    setKind("embedding");
    setScope(selectedIds.length > 0 ? "selected" : query ? "view" : "all");
    setOverwrite(false);
    setPrompt(DEFAULT_DESCRIPTION_PROMPT);
    setMaxTokens(160);
    // 重新打开对话框时接管仍在运行的任务，避免进度空白。
    setJob(statusRef.current?.job ?? null);
    setError(null);
    setStarting(false);
    setEnsuring(false);
    finishedRef.current = false;
    // 只在开关状态变化时重置表单；状态轮询不应清空用户输入。
  }, [open]);

  useEffect(() => {
    if (!open || aiBrowserPreview) return;
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    void (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      const progress = await listen<AiJobProgress>("ai-job-progress", (event) => {
        if (disposed) return;
        const current = jobRef.current;
        if (!current || current.jobId === event.payload.jobId) {
          setJob((previous) =>
            previous ? { ...previous, ...event.payload } : previous,
          );
        }
      });
      const finished = await listen<AiJobSnapshot>("ai-job-finished", (event) => {
        if (disposed) return;
        const current = jobRef.current;
        if (current && current.jobId !== event.payload.jobId) return;
        setJob(event.payload);
        if (!finishedRef.current) {
          finishedRef.current = true;
          onFinished();
        }
      });
      unlisteners.push(progress, finished);
    })();
    return () => {
      disposed = true;
      for (const unlisten of unlisteners) unlisten();
    };
  }, [open, onFinished]);

  const capability = status?.health?.capabilities[CAPABILITY[kind]];
  const capabilityReady = capability?.ready === true;

  const ensureModel = async () => {
    setEnsuring(true);
    setError(null);
    try {
      await aiApi.ensureModels([CAPABILITY[kind]]);
      await onRefreshStatus();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setEnsuring(false);
    }
  };

  const service = async (action: "start" | "stop" | "refresh") => {
    setServiceBusy(true);
    setError(null);
    try {
      if (action === "start") await aiApi.startService();
      if (action === "stop") await aiApi.stopService();
      await onRefreshStatus();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setServiceBusy(false);
    }
  };

  const start = async () => {
    setStarting(true);
    setError(null);
    setJob(null);
    finishedRef.current = false;
    try {
      const request =
        scope === "selected"
          ? {
              kind,
              imageIds: selectedIds,
              query: null,
              overwrite,
              prompt: kind === "description" ? prompt : null,
              maxTokens: kind === "description" ? maxTokens : null,
            }
          : {
              kind,
              imageIds: [],
              query: scope === "view" ? query : null,
              overwrite,
              prompt: kind === "description" ? prompt : null,
              maxTokens: kind === "description" ? maxTokens : null,
            };
      const started = await aiApi.startJob(request);
      setJob(started);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setStarting(false);
    }
  };

  const cancel = async () => {
    if (!job) return;
    try {
      await aiApi.cancelJob(job.jobId);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const footer = running ? (
    <Button variant="danger" icon={<Square size={13} />} onClick={() => void cancel()}>
      取消任务
    </Button>
  ) : (
    <>
      <Button variant="ghost" onClick={onClose}>
        关闭
      </Button>
      <Button
        variant="primary"
        icon={<Play size={13} />}
        loading={starting}
        disabled={scope === "selected" && selectedIds.length === 0}
        onClick={() => void start()}
      >
        开始任务
      </Button>
    </>
  );

  return (
    <Modal
      open={open}
      title="AI 任务"
      subtitle="批量调用本机 AI 服务；服务离线不会影响图库浏览与编辑。"
      onClose={onClose}
      width={660}
      dismissible={!running}
      footer={footer}
    >
      <div className="ai-dialog">
        <div className={`ai-service-status${status?.online ? " is-online" : ""}`}>
          <div>
            <strong>{status?.online ? "AI 服务在线" : "AI 服务离线"}</strong>
            <span>
              {status?.process.message ??
                status?.process.state ??
                "正在读取服务状态…"}
            </span>
            {status?.health ? (
              <span>
                模型：
                {Object.entries(status.health.capabilities)
                  .filter(([, value]) => value.ready)
                  .map(([name]) => name)
                  .join(" · ") || "没有已就绪能力"}
              </span>
            ) : null}
          </div>
          <div className="ai-service-actions">
            <Button
              size="sm"
              variant="ghost"
              icon={<RefreshCw size={13} />}
              loading={serviceBusy}
              onClick={() => void service("refresh")}
            >
              检测
            </Button>
            {status?.process.owned ? (
              <Button
                size="sm"
                variant="ghost"
                loading={serviceBusy}
                onClick={() => void service("stop")}
              >
                停止服务
              </Button>
            ) : (
              <Button
                size="sm"
                variant="secondary"
                loading={serviceBusy}
                disabled={!status?.executableConfigured || aiBrowserPreview}
                onClick={() => void service("start")}
              >
                启动服务
              </Button>
            )}
          </div>
        </div>

        <div className="ai-kind-grid" role="radiogroup" aria-label="任务类型">
          {(Object.keys(KIND_LABELS) as AiJobKind[]).map((value) => (
            <button
              key={value}
              type="button"
              role="radio"
              aria-checked={kind === value}
              className={`ai-kind${kind === value ? " is-active" : ""}`}
              disabled={running}
              onClick={() => setKind(value)}
            >
              <Sparkles size={14} />
              <span>{KIND_LABELS[value]}</span>
            </button>
          ))}
        </div>

        <div className="ai-capability">
          <span>
            {capability
              ? `${capability.model || "未命名模型"}${capability.dim ? ` · ${capability.dim} 维` : ""}`
              : "服务未报告该能力"}
          </span>
          {capabilityReady ? (
            <span className="ai-ready">
              <CheckCircle2 size={13} /> 已就绪
            </span>
          ) : (
            <Button
              size="sm"
              variant="secondary"
              loading={ensuring}
              disabled={!status?.online || aiBrowserPreview}
              onClick={() => void ensureModel()}
            >
              加载模型
            </Button>
          )}
        </div>

        <Field label="处理范围">
          <div className="ai-scope-list">
            <label>
              <input
                type="radio"
                name="ai-scope"
                checked={scope === "selected"}
                disabled={running || selectedIds.length === 0}
                onChange={() => setScope("selected")}
              />
              选中图片（{selectedIds.length.toLocaleString()} 张）
            </label>
            <label>
              <input
                type="radio"
                name="ai-scope"
                checked={scope === "view"}
                disabled={running || !query}
                onChange={() => setScope("view")}
              />
              当前视图（筛选后的全部图片）
            </label>
            <label>
              <input
                type="radio"
                name="ai-scope"
                checked={scope === "all"}
                disabled={running}
                onChange={() => setScope("all")}
              />
              整个图库
            </label>
          </div>
        </Field>

        {kind === "description" ? (
          <div className="settings-grid">
            <Field label="提示词" hint="留空使用服务端默认提示。">
              <textarea
                className="input ai-prompt"
                value={prompt}
                disabled={running}
                onChange={(event) => setPrompt(event.target.value)}
              />
            </Field>
            <Field label="最大长度" hint="16-512 tokens。">
              <input
                className="input"
                type="number"
                min={16}
                max={512}
                value={maxTokens}
                disabled={running}
                onChange={(event) => setMaxTokens(Number(event.target.value))}
              />
            </Field>
          </div>
        ) : null}

        <label className="toggle-row">
          <input
            type="checkbox"
            checked={overwrite}
            disabled={running}
            onChange={(event) => setOverwrite(event.target.checked)}
          />
          <span>
            <strong>
              {kind === "description"
                ? "重新生成已有 AI/导入描述"
                : "重新生成已存在的特征"}
            </strong>
            <small>
              {kind === "description"
                ? "手写描述始终不会被覆盖；需要替换时先手动清空。"
                : "不勾选时只处理缺失项；勾选后会追加新记录并更新指针。"}
            </small>
          </span>
        </label>

        {job ? (
          <div className="ai-job">
            <div className="ai-job-head">
              <strong>
                {KIND_LABELS[job.kind]} · {PHASE_LABELS[job.phase]}
              </strong>
              <span>
                {job.completed.toLocaleString()} / {job.total.toLocaleString()}
                {job.failed > 0 ? ` · 失败 ${job.failed}` : ""}
                {job.skipped > 0 ? ` · 跳过 ${job.skipped}` : ""}
              </span>
            </div>
            <div className="ai-progress">
              <span
                style={{
                  width: `${
                    job.total > 0
                      ? Math.min(
                          100,
                          ((job.completed + job.failed + job.skipped) / job.total) * 100,
                        )
                      : 0
                  }%`,
                }}
              />
            </div>
            <div className="ai-job-meta">
              <span>{job.message || (job.currentImageId ? `#${job.currentImageId}` : "—")}</span>
              <span>{(job.elapsedMs / 1000).toFixed(1)}s</span>
            </div>
            {job.failures.length > 0 ? (
              <div className="ai-failures">
                <span>
                  <AlertTriangle size={13} /> 失败 {job.failed} 项
                </span>
                {job.failures.slice(0, 8).map((failure, index) => (
                  <code key={`${failure.imageId}-${index}`}>
                    {failure.imageId ? `#${failure.imageId} ` : ""}
                    {failure.reason}
                  </code>
                ))}
              </div>
            ) : null}
          </div>
        ) : null}

        {error ? <div className="form-error">{error}</div> : null}
        {starting ? (
          <div className="ai-starting">
            <Spinner size={14} /> 正在准备任务…
          </div>
        ) : null}
      </div>
    </Modal>
  );
}

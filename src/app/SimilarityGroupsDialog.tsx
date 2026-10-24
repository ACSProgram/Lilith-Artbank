import { useEffect, useState } from "react";
import { Layers, Plus, ScanSearch, Square } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Modal } from "../shared/components/Modal";
import { Spinner } from "../shared/components/Spinner";
import { aiApi } from "../modules/ai/api";
import { libraryApi } from "../modules/library/api";
import type { SimilarityGroupsResult } from "../modules/ai/types";
import { errorMessage } from "../shared/errors";
import { thumbnailUrl } from "../shared/thumbUrl";
import { useNotices } from "./noticeContext";
import { useTaskCenter } from "./taskContext";
import { taskApi } from "./taskApi";
import { TASK_PHASE_LABELS, isTaskActive, taskPercent } from "./taskTypes";

interface SimilarityGroupsDialogProps {
  open: boolean;
  onClose: () => void;
  onChanged: () => void;
  /** 已经启动的扫描；打开时直接接管它的进度与结果。 */
  initialTaskId?: string | null;
  /** 在对话框里启动扫描后把任务 id 交给上层，关掉再打开仍能接回进度。 */
  onTaskStarted?: (taskId: string) => void;
}

const VISIBLE_GROUPS = 200;

function metricLabel(result: SimilarityGroupsResult): string {
  if (result.phash && result.embedding) return "pHash + 语义";
  return result.phash ? "pHash" : "语义";
}

export function SimilarityGroupsDialog({
  open,
  onClose,
  onChanged,
  initialTaskId = null,
  onTaskStarted,
}: SimilarityGroupsDialogProps) {
  const { notify } = useNotices();
  const { start, cancel, find } = useTaskCenter();
  const [phashOn, setPhashOn] = useState(true);
  const [phashThreshold, setPhashThreshold] = useState(6);
  const [embeddingOn, setEmbeddingOn] = useState(false);
  const [embeddingThreshold, setEmbeddingThreshold] = useState(0.97);
  const [baseName, setBaseName] = useState("相似组");
  const [starting, setStarting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<SimilarityGroupsResult | null>(null);
  const [taskId, setTaskId] = useState<string | null>(null);
  const [embeddingCount, setEmbeddingCount] = useState<number | null>(null);

  useEffect(() => {
    if (!open) return;
    setStarting(false);
    setSaving(false);
    setError(null);
    setResult(null);
    setTaskId(initialTaskId);
  }, [open, initialTaskId]);

  useEffect(() => {
    if (!open) return;
    let disposed = false;
    void aiApi
      .getStatus()
      .then((status) => {
        if (!disposed) setEmbeddingCount(status.mean.count);
      })
      .catch(() => {
        if (!disposed) setEmbeddingCount(0);
      });
    return () => {
      disposed = true;
    };
  }, [open]);

  const task = taskId ? find(taskId) : null;
  const running = task ? isTaskActive(task) : false;
  const percent = task ? taskPercent(task) : 0;
  const embeddingAvailable = (embeddingCount ?? 0) > 0;
  const ready = phashOn || (embeddingOn && embeddingAvailable);

  useEffect(() => {
    if (!task || isTaskActive(task)) return;
    if (task.phase === "failed") {
      setError(task.message || "扫描失败");
      return;
    }
    if (task.phase !== "completed") return;
    let disposed = false;
    void taskApi
      .similarityGroupsResult(task.taskId)
      .then((value) => {
        if (disposed) return;
        if (value) setResult(value);
        else setError("扫描结果已失效，请重新扫描");
      })
      .catch((cause: unknown) => {
        if (!disposed) setError(errorMessage(cause));
      });
    return () => {
      disposed = true;
    };
  }, [task]);

  const scan = async () => {
    if (running || starting || !ready) return;
    setStarting(true);
    setError(null);
    setResult(null);
    try {
      const started = await start({
        kind: "similarityGroups",
        scope: { type: "library" },
        overwrite: false,
        recalcVisuals: false,
        prompt: null,
        maxTokens: null,
        groupPhash: phashOn,
        phashThreshold: phashOn ? phashThreshold : null,
        groupEmbedding: embeddingOn && embeddingAvailable,
        embeddingThreshold:
          embeddingOn && embeddingAvailable ? embeddingThreshold : null,
        conflict: "queue",
      });
      if (started.status === "conflict") {
        setError("暂时无法开始扫描，请稍后再试");
        return;
      }
      setTaskId(started.task.taskId);
      onTaskStarted?.(started.task.taskId);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setStarting(false);
    }
  };

  const createCollection = async (name: string, ids: number[]) => {
    const collection = await libraryApi.createCollection(name);
    await libraryApi.addImagesToCollection(collection.id, ids);
  };

  const uniqueName = (used: Set<string>, base: string, index: number) => {
    const prefix = base.trim() || "相似组";
    let name = `${prefix} ${index + 1}`;
    let suffix = 2;
    while (used.has(name.toLowerCase())) {
      name = `${prefix} ${index + 1}-${suffix}`;
      suffix += 1;
    }
    used.add(name.toLowerCase());
    return name;
  };

  const saveGroups = async (groups: number[][]) => {
    if (saving || groups.length === 0) return;
    if (groups.length > 100) {
      notify("一次最多创建 100 个集合", "error", "可以改用“合并保存”，或分批保存。");
      return;
    }
    setSaving(true);
    try {
      const collections = await libraryApi.getCollections();
      const used = new Set(collections.map((collection) => collection.name.toLowerCase()));
      for (let index = 0; index < groups.length; index += 1) {
        const name = uniqueName(used, baseName, index);
        await createCollection(name, groups[index]);
      }
      notify(`已创建 ${groups.length} 个集合`, "success");
      onChanged();
    } catch (cause) {
      notify("保存相似组失败", "error", errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };

  const mergeGroups = async () => {
    if (saving || !result || result.groups.length === 0) return;
    setSaving(true);
    try {
      const ids = [...new Set(result.groups.flat())];
      const collections = await libraryApi.getCollections();
      const used = new Set(collections.map((collection) => collection.name.toLowerCase()));
      const name = uniqueName(used, baseName, 0);
      await createCollection(name, ids);
      notify(`已把 ${ids.length.toLocaleString()} 张图片保存为集合`, "success");
      onChanged();
    } catch (cause) {
      notify("保存相似组失败", "error", errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };

  const visible = result?.groups.slice(0, VISIBLE_GROUPS) ?? [];

  return (
    <Modal
      open={open}
      title="相似分组"
      subtitle="扫描重复或高度相似的图片，结果可以存成集合。"
      width={760}
      onClose={onClose}
      footer={
        <Button variant="primary" onClick={onClose}>
          完成
        </Button>
      }
    >
      <section className="dialog-section">
        <h3>
          <ScanSearch size={14} /> 比较方式
        </h3>

        <div className="similarity-metrics">
          <div className={`similarity-metric${phashOn ? "" : " is-off"}`}>
            <label className="similarity-metric-head">
              <input
                type="checkbox"
                checked={phashOn}
                disabled={running}
                onChange={(event) => setPhashOn(event.target.checked)}
              />
              <span>
                <strong>pHash</strong>
                <small>画面结构</small>
              </span>
            </label>
            <label className="similarity-metric-control">
              <span>差异上限</span>
              <input
                type="range"
                min={0}
                max={7}
                step={1}
                value={phashThreshold}
                disabled={!phashOn || running}
                onChange={(event) => setPhashThreshold(Number(event.target.value))}
              />
              <em>{phashThreshold}</em>
            </label>
          </div>

          <div className={`similarity-metric${embeddingOn ? "" : " is-off"}`}>
            <label
              className="similarity-metric-head"
              title={
                embeddingAvailable
                  ? undefined
                  : "还没有图片 embedding，可以先在批量任务里生成。"
              }
            >
              <input
                type="checkbox"
                checked={embeddingOn}
                disabled={running || !embeddingAvailable}
                onChange={(event) => setEmbeddingOn(event.target.checked)}
              />
              <span>
                <strong>语义</strong>
                <small>画面内容</small>
              </span>
            </label>
            <label className="similarity-metric-control">
              <span>相似度下限</span>
              <input
                type="range"
                min={0.5}
                max={1}
                step={0.01}
                value={embeddingThreshold}
                disabled={!embeddingOn || !embeddingAvailable || running}
                onChange={(event) => setEmbeddingThreshold(Number(event.target.value))}
              />
              <em>{embeddingThreshold.toFixed(2)}</em>
            </label>
          </div>
        </div>

        <div className="dialog-inline">
          <p className="dialog-hint">
            {phashOn && embeddingOn
              ? "同时开启时，两个条件都满足才归为一组。"
              : phashOn
                ? "按画面结构分组。"
                : embeddingAvailable
                  ? "按画面内容分组。"
                  : "还需要图片的语义特征才能按内容分组。"}
          </p>
          {running ? (
            <Button
              size="sm"
              variant="danger"
              icon={<Square size={12} />}
              onClick={() => taskId && void cancel(taskId)}
            >
              取消扫描
            </Button>
          ) : (
            <Button
              size="sm"
              variant="primary"
              icon={<ScanSearch size={13} />}
              loading={starting}
              disabled={!ready}
              title={ready ? undefined : "至少开启一种比较方式"}
              onClick={() => void scan()}
            >
              扫描
            </Button>
          )}
        </div>

        {task ? (
          <div className="similarity-scan-progress">
            <div className="similarity-scan-head">
              <span>
                {isTaskActive(task) ? <Spinner size={12} /> : null}
                {TASK_PHASE_LABELS[task.phase]}
                {task.message ? ` · ${task.message}` : ""}
              </span>
              <em>{percent}%</em>
            </div>
            <div className="task-progress">
              <span style={{ width: `${percent}%` }} />
            </div>
          </div>
        ) : null}

        {error ? <p className="dialog-error">{error}</p> : null}
      </section>

      {result ? (
        <section className="dialog-section">
          <h3>
            <Layers size={14} /> 结果
          </h3>
          <div className="dialog-inline">
            <span className="dialog-hint">
              {metricLabel(result)} · {result.groups.length.toLocaleString()} 组
              {result.missing > 0 ? ` · ${result.missing.toLocaleString()} 张缺少数据` : ""}
              {result.truncated ? " · 已截断" : ""}
            </span>
            <input
              className="input"
              value={baseName}
              maxLength={60}
              disabled={saving}
              onChange={(event) => setBaseName(event.target.value)}
              aria-label="集合名称前缀"
            />
            <Button
              size="sm"
              variant="secondary"
              icon={<Plus size={13} />}
              loading={saving}
              disabled={result.groups.length === 0}
              onClick={() => void mergeGroups()}
            >
              合并保存
            </Button>
            <Button
              size="sm"
              variant="secondary"
              icon={<Layers size={13} />}
              loading={saving}
              disabled={result.groups.length === 0 || result.groups.length > 100}
              title={
                result.groups.length > 100
                  ? "一次最多创建 100 个集合，请改用合并保存"
                  : undefined
              }
              onClick={() => void saveGroups(result.groups)}
            >
              每组一个集合
            </Button>
          </div>
          {visible.length === 0 ? (
            <p className="dialog-hint">没有找到重复或相似组。</p>
          ) : (
            <div className="similarity-group-list">
              {visible.map((ids, index) => (
                <div className="similarity-group-row" key={`${index}-${ids[0]}`}>
                  <span className="similarity-group-index">第 {index + 1} 组</span>
                  <span className="similarity-group-count">{ids.length.toLocaleString()} 张</span>
                  <span className="similarity-group-thumbs">
                    {ids.slice(0, 6).map((id) => (
                      <img key={id} src={thumbnailUrl(id)} alt="" loading="lazy" />
                    ))}
                    {ids.length > 6 ? <em>+{ids.length - 6}</em> : null}
                  </span>
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={saving}
                    onClick={() => void saveGroups([ids])}
                  >
                    存为集合
                  </Button>
                </div>
              ))}
            </div>
          )}
          {result.groups.length > VISIBLE_GROUPS ? (
            <p className="dialog-hint">
              仅显示前 {VISIBLE_GROUPS} 组；批量按钮会处理全部 {result.groups.length.toLocaleString()} 组。
            </p>
          ) : null}
        </section>
      ) : null}
    </Modal>
  );
}

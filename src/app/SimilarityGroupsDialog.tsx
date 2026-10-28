import { useEffect, useMemo, useState } from "react";
import { Layers, Plus, ScanSearch, Square } from "lucide-react";
import { Button } from "../shared/components/Button";
import { Modal } from "../shared/components/Modal";
import { ScopePicker } from "../shared/components/ScopePicker";
import { Spinner } from "../shared/components/Spinner";
import { aiApi } from "../modules/ai/api";
import { libraryApi } from "../modules/library/api";
import { keepSortSummary } from "../modules/library/model";
import type { SimilarityGroupsResult } from "../modules/ai/types";
import type { SortSpec } from "../modules/library/types";
import type { BatchTaskContextInfo } from "../shared/batchTaskContext";
import { confirmMergeSimilarGroups } from "../shared/confirm";
import { errorMessage } from "../shared/errors";
import { buildScope, scopeAvailability, type ScopeChoice } from "../shared/scope";
import { thumbnailUrl } from "../shared/thumbUrl";
import { KeepSortPicker } from "./KeepSortPicker";
import { useNotices } from "./noticeContext";
import { useTaskCenter } from "./taskContext";
import { taskApi } from "./taskApi";
import { TASK_PHASE_LABELS, isTaskActive, taskPercent } from "./taskTypes";

interface SimilarityGroupsDialogProps {
  open: boolean;
  onClose: () => void;
  onChanged: () => void;
  /** 合并把图片移入回收站后通知工作区，让它按范围变化刷新。 */
  onMerged: (imageIds: number[]) => void;
  /** 当前浏览状态，用于范围选择与计数。 */
  context: BatchTaskContextInfo | null;
  /** 合并时每组保留哪一张的规则；存在设置里。 */
  keepSort: SortSpec[];
  onKeepSortChange: (next: SortSpec[]) => void;
  /** 已经启动的扫描；打开时直接接管它的进度与结果。 */
  initialTaskId?: string | null;
  /** 在对话框里启动扫描后把任务 id 交给上层，关掉再打开仍能接回进度。 */
  onTaskStarted?: (taskId: string | null) => void;
}

/** 每组预览的缩略图张数。 */
const GROUP_THUMBS = 6;
/** 一页显示多少组；结果本身已经全部在前端，翻页只影响渲染。 */
const PAGE_SIZE = 50;

function metricLabel(result: SimilarityGroupsResult): string {
  if (result.phash && result.embedding) return "pHash + 语义";
  return result.phash ? "pHash" : "语义";
}

export function SimilarityGroupsDialog({
  open,
  onClose,
  onChanged,
  onMerged,
  context,
  keepSort,
  onKeepSortChange,
  initialTaskId = null,
  onTaskStarted,
}: SimilarityGroupsDialogProps) {
  const { notify, confirm } = useNotices();
  const { start, cancel, find } = useTaskCenter();
  const [phashOn, setPhashOn] = useState(true);
  const [phashThreshold, setPhashThreshold] = useState(6);
  const [embeddingOn, setEmbeddingOn] = useState(false);
  const [embeddingThreshold, setEmbeddingThreshold] = useState(0.97);
  const [scopeChoice, setScopeChoice] = useState<ScopeChoice>("library");
  const [baseName, setBaseName] = useState("相似组");
  const [starting, setStarting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [merging, setMerging] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<SimilarityGroupsResult | null>(null);
  const [taskId, setTaskId] = useState<string | null>(null);
  const [embeddingCount, setEmbeddingCount] = useState<number | null>(null);
  const [page, setPage] = useState(0);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [keeps, setKeeps] = useState<(number | null)[]>([]);

  // 每次打开都从干净状态开始；结果由 initialTaskId 决定要不要接回。
  useEffect(() => {
    if (!open) return;
    setScopeChoice("library");
    setStarting(false);
    setSaving(false);
    setMerging(false);
    setError(null);
    setResult(null);
    setPage(0);
    setSelected(new Set());
  }, [open]);

  // 任务 id 单独跟随：合并后会把两边都清成 null，但已经算出来的剩余组要留在界面上。
  useEffect(() => {
    if (!open) return;
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

  const groups = useMemo(() => result?.groups ?? [], [result]);
  const pageCount = Math.max(1, Math.ceil(groups.length / PAGE_SIZE));
  const currentPage = Math.min(page, pageCount - 1);
  const pageStart = currentPage * PAGE_SIZE;
  const pageGroups = useMemo(
    () => groups.slice(pageStart, pageStart + PAGE_SIZE),
    [groups, pageStart],
  );

  // 结果换了就回到第一页，并清掉上一份结果的勾选。
  useEffect(() => {
    setPage(0);
    setSelected(new Set());
  }, [result]);

  const keepSortKey = useMemo(() => JSON.stringify(keepSort), [keepSort]);

  // 只给当前页算「保留哪一张」用于行内标记；合并时按选中的组再算一次。
  useEffect(() => {
    if (!open || pageGroups.length === 0) {
      setKeeps([]);
      return;
    }
    let disposed = false;
    void libraryApi
      .similarityGroupKeeps(pageGroups, keepSort)
      .then((value) => {
        if (!disposed) setKeeps(value);
      })
      .catch(() => {
        if (!disposed) setKeeps([]);
      });
    return () => {
      disposed = true;
    };
    // keepSort 用序列化后的值做依赖，避免每次渲染都重新请求。
  }, [open, pageGroups, keepSortKey]);

  const availability = {
    ...scopeAvailability(context),
    // 临时图片不参与分组，也不能进回收站；给了这个选项只会在合并时报错。
    temp: { enabled: false, reason: "临时图片不参与相似分组" } as const,
  };

  const scan = async () => {
    if (running || starting || !ready) return;
    // 全库范围不依赖浏览状态；其它范围必须有上下文才能解析。
    const scope =
      scopeChoice === "library"
        ? ({ type: "library" } as const)
        : context
          ? buildScope(scopeChoice, context)
          : null;
    if (!scope) {
      setError("无法读取当前范围，请重新选择");
      return;
    }
    setStarting(true);
    setError(null);
    setResult(null);
    try {
      const started = await start({
        kind: "similarityGroups",
        scope,
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

  const saveGroups = async (groupsToSave: number[][]) => {
    if (saving || groupsToSave.length === 0) return;
    if (groupsToSave.length > 100) {
      notify("一次最多创建 100 个集合", "error", "可以改用“合并保存”，或分批保存。");
      return;
    }
    setSaving(true);
    try {
      const created = await libraryApi.createCollectionsFromGroups(groupsToSave, baseName);
      notify(`已创建 ${created.length} 个集合`, "success");
      onChanged();
    } catch (cause) {
      notify("保存相似组失败", "error", errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };

  const mergeIntoCollection = async () => {
    if (saving || groups.length === 0) return;
    const ids = [...new Set(groups.flat())];
    if (ids.length === 0) return;
    setSaving(true);
    try {
      await libraryApi.createCollectionsFromGroups([ids], baseName);
      notify(`已把 ${ids.length.toLocaleString()} 张图片保存为集合`, "success");
      onChanged();
    } catch (cause) {
      notify("保存相似组失败", "error", errorMessage(cause));
    } finally {
      setSaving(false);
    }
  };

  const toggleGroup = (index: number) => {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  };

  const allSelected = groups.length > 0 && selected.size === groups.length;

  const toggleAll = () => {
    setSelected(allSelected ? new Set() : new Set(groups.map((_, index) => index)));
  };

  const selectedImageCount = [...selected].reduce(
    (total, index) => total + (groups[index]?.length ?? 0),
    0,
  );

  const mergeSelected = async () => {
    if (merging || saving || selected.size === 0 || !result) return;
    const picked = [...selected].sort((left, right) => left - right);
    const pickedGroups = picked.map((index) => groups[index]);
    if (pickedGroups.length === 0) return;
    setMerging(true);
    try {
      // 以合并这一刻的排序为准重新挑一次保留项，不用页面上可能过期的标记。
      const keepsNow = await libraryApi.similarityGroupKeeps(pickedGroups, keepSort);
      const kept = new Set(keepsNow.filter((id): id is number => id != null));
      const trashIds: number[] = [];
      const seen = new Set<number>();
      for (const group of pickedGroups) {
        for (const id of group) {
          if (kept.has(id) || seen.has(id)) continue;
          seen.add(id);
          trashIds.push(id);
        }
      }
      if (trashIds.length === 0) {
        notify("选中的组里没有可移入回收站的图片", "info");
        return;
      }
      const ok = await confirmMergeSimilarGroups(
        confirm,
        pickedGroups.length,
        trashIds.length,
        keepSortSummary(keepSort),
      );
      if (!ok) return;
      const changed = await libraryApi.trashImages(trashIds);
      notify(
        `已把 ${changed.toLocaleString()} 张图片移入回收站`,
        "success",
        "已处理的组从结果里移除；剩下的组可以接着处理，重新扫描会重新计算。可以在操作记录里整批撤销。",
      );
      // 组之间互不重叠，处理过的组直接从结果里去掉；剩下的组仍然有效。
      const remaining = groups.filter((_, index) => !selected.has(index));
      setResult(remaining.length > 0 ? { ...result, groups: remaining } : null);
      // 旧任务结果不再对应这份列表，重开对话框时不要把它读回来。
      setTaskId(null);
      onTaskStarted?.(null);
      onMerged(trashIds);
    } catch (cause) {
      notify("合并相似组失败", "error", errorMessage(cause));
    } finally {
      setMerging(false);
    }
  };

  return (
    <Modal
      open={open}
      title="相似分组"
      subtitle="扫描重复或高度相似的图片，结果可以存成集合，也可以每组只留一张。"
      width={820}
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

        <p className="dialog-hint">
          {phashOn && embeddingOn
            ? "同时开启时，两个条件都满足才归为一组。"
            : phashOn
              ? "按画面结构分组。"
              : embeddingAvailable
                ? "按画面内容分组。"
                : "还需要图片的语义特征才能按内容分组。"}
        </p>
      </section>

      <section className="dialog-section">
        <h3>
          <Layers size={14} /> 范围
        </h3>
        <ScopePicker
          name="similarity-groups-scope"
          value={scopeChoice}
          onChange={setScopeChoice}
          context={context}
          availability={availability}
        />
        <p className="dialog-hint">只在范围内的图片之间比较，组内不会出现范围外的图片。</p>

        <div className="dialog-inline">
          <span className="dialog-hint">
            {running ? "扫描中…" : "选好范围和比较方式后开始扫描。"}
          </span>
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
              disabled={groups.length === 0}
              onClick={() => void mergeIntoCollection()}
            >
              合并保存
            </Button>
            <Button
              size="sm"
              variant="secondary"
              icon={<Layers size={13} />}
              loading={saving}
              disabled={groups.length === 0 || groups.length > 100}
              title={
                groups.length > 100
                  ? "一次最多创建 100 个集合，请改用合并保存"
                  : undefined
              }
              onClick={() => void saveGroups(groups)}
            >
              每组一个集合
            </Button>
          </div>

          {groups.length === 0 ? (
            <p className="dialog-hint">没有找到重复或相似组。</p>
          ) : (
            <>
              <KeepSortPicker
                sort={keepSort}
                onChange={onKeepSortChange}
                disabled={merging}
              />

              <div className="similarity-group-toolbar">
                <label className="similarity-group-selectall">
                  <input
                    type="checkbox"
                    checked={allSelected}
                    disabled={merging}
                    onChange={toggleAll}
                  />
                  <span>全选 {groups.length.toLocaleString()} 组</span>
                </label>
                <span className="dialog-hint">
                  已选 {selected.size.toLocaleString()} 组 ·{" "}
                  {selectedImageCount.toLocaleString()} 张
                </span>
                <Button
                  size="sm"
                  variant="primary"
                  loading={merging}
                  disabled={selected.size === 0}
                  title={
                    selected.size === 0 ? "先勾选要合并的组" : keepSortSummary(keepSort)
                  }
                  onClick={() => void mergeSelected()}
                >
                  合并选中
                </Button>
              </div>

              <div className="similarity-group-list">
                {pageGroups.map((ids, localIndex) => {
                  const index = pageStart + localIndex;
                  const rawKeep = keeps[localIndex] ?? null;
                  // 翻页后上一页的保留项会短暂留在状态里，必须确认它属于这一组。
                  const keepId = rawKeep != null && ids.includes(rawKeep) ? rawKeep : null;
                  const ordered =
                    keepId != null
                      ? [keepId, ...ids.filter((id) => id !== keepId)]
                      : ids;
                  return (
                    <label
                      className={`similarity-group-row${
                        selected.has(index) ? " is-selected" : ""
                      }`}
                      key={`${index}-${ids[0]}`}
                    >
                      <input
                        type="checkbox"
                        checked={selected.has(index)}
                        disabled={merging}
                        onChange={() => toggleGroup(index)}
                      />
                      <span className="similarity-group-index">第 {index + 1} 组</span>
                      <span className="similarity-group-count">
                        {ids.length.toLocaleString()} 张
                      </span>
                      <span className="similarity-group-thumbs">
                        {ordered.slice(0, GROUP_THUMBS).map((id) => (
                          <span className="similarity-group-thumb" key={id}>
                            <img src={thumbnailUrl(id)} alt="" loading="lazy" />
                            {id === keepId ? (
                              <em className="similarity-group-keep">保留</em>
                            ) : null}
                          </span>
                        ))}
                        {ids.length > GROUP_THUMBS ? (
                          <em>+{ids.length - GROUP_THUMBS}</em>
                        ) : null}
                      </span>
                      <Button
                        type="button"
                        size="sm"
                        variant="secondary"
                        disabled={saving || merging}
                        onClick={(event) => {
                          // 行本身是 label，点按钮不要再把这一行勾上。
                          event.preventDefault();
                          void saveGroups([ids]);
                        }}
                      >
                        存为集合
                      </Button>
                    </label>
                  );
                })}
              </div>

              {pageCount > 1 ? (
                <div className="similarity-pager">
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={currentPage === 0}
                    onClick={() => setPage(currentPage - 1)}
                  >
                    上一页
                  </Button>
                  <span className="dialog-hint">
                    第 {currentPage + 1} / {pageCount} 页 · 每页 {PAGE_SIZE} 组
                  </span>
                  <Button
                    size="sm"
                    variant="secondary"
                    disabled={currentPage >= pageCount - 1}
                    onClick={() => setPage(currentPage + 1)}
                  >
                    下一页
                  </Button>
                </div>
              ) : null}
            </>
          )}
        </section>
      ) : null}
    </Modal>
  );
}

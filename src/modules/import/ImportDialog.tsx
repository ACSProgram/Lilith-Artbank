import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, FolderOpen, ImagePlus, Images, UploadCloud } from "lucide-react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { Button } from "../../shared/components/Button";
import { Field } from "../../shared/components/Field";
import { FolderPickerDialog } from "../../shared/components/FolderPickerDialog";
import { Modal } from "../../shared/components/Modal";
import { TagInput, type TagInputHandle } from "../../shared/components/TagInput";
import { thumbnailUrl } from "../../shared/thumbUrl";
import { formatBytes } from "../../shared/format";
import { confirmImportOptimize } from "../../shared/confirm";
import { useNotices } from "../../app/noticeContext";
import { importApi } from "./api";
import type {
  DuplicatePolicy,
  FolderMode,
  ImportFolder,
  ImportOptimizeFormat,
  ImportOptimizeTier,
  ImportPhase,
  ImportProgress,
  ImportSnapshot,
  ImportTarget,
} from "./types";
import { errorMessage } from "../../shared/errors";

const IMAGE_EXTENSIONS = ["jpg", "jpeg", "png", "bmp", "webp", "tif", "tiff", "tga", "gif"];

const OPTIMIZE_TIERS: Array<{ value: ImportOptimizeTier; label: string }> = [
  { value: "extreme", label: "极高" },
  { value: "high", label: "高" },
  { value: "medium", label: "中" },
];

type View = "options" | "running" | "done";

interface ImportDialogProps {
  open: boolean;
  initialPaths: string[];
  /** 打开时预填的目标文件夹（当前浏览的文件夹）。 */
  initialFolder?: string;
  /** 导入到临时文件夹时不写操作记录、不建目录层级，关闭图库时清空。 */
  target?: ImportTarget;
  folders: ImportFolder[];
  /** 标签自动补全；由 app 层注入，避免 import 模块依赖 library 模块。 */
  searchTags?: (query: string) => Promise<string[]>;
  onClose: () => void;
  onFinished: () => void;
}

function basename(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

function phaseLabel(phase: ImportPhase): string {
  switch (phase) {
    case "queued":
      return "排队中";
    case "scanning":
      return "正在扫描输入目录";
    case "processing":
      return "正在导入";
    case "finishing":
      return "正在收尾";
    case "completed":
      return "导入完成";
    case "cancelled":
      return "导入已取消";
    case "failed":
      return "导入失败";
  }
}

function isTerminal(phase: ImportPhase): boolean {
  return phase === "completed" || phase === "cancelled" || phase === "failed";
}

function formatDuration(milliseconds: number): string {
  if (milliseconds < 1000) return `${milliseconds} ms`;
  const seconds = milliseconds / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)} 秒`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes} 分 ${Math.round(seconds % 60)} 秒`;
}

export function ImportDialog({
  open,
  initialPaths,
  initialFolder = "",
  target = "library",
  folders,
  searchTags,
  onClose,
  onFinished,
}: ImportDialogProps) {
  const { confirm } = useNotices();
  const temp = target === "temp";
  const [paths, setPaths] = useState<string[]>([]);
  const [folder, setFolder] = useState("");
  const [folderMode, setFolderMode] = useState<FolderMode>("flat");
  const [folderPickerOpen, setFolderPickerOpen] = useState(false);
  const [tags, setTags] = useState<string[]>([]);
  const tagInputRef = useRef<TagInputHandle>(null);
  const [score, setScore] = useState(0);
  const [policy, setPolicy] = useState<DuplicatePolicy>("skip");
  const [optimizeEnabled, setOptimizeEnabled] = useState(false);
  const [optimizeFormat, setOptimizeFormat] = useState<ImportOptimizeFormat>("jpeg");
  const [optimizeTier, setOptimizeTier] = useState<ImportOptimizeTier>("high");
  const [view, setView] = useState<View>("options");
  const [jobId, setJobId] = useState<string | null>(null);
  const [progress, setProgress] = useState<ImportProgress | null>(null);
  const [summary, setSummary] = useState<ImportSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  const jobIdRef = useRef<string | null>(null);
  const initialRef = useRef(initialPaths);
  const initialFolderRef = useRef(initialFolder);
  const finishedRef = useRef(false);
  const viewRef = useRef<View>("options");
  const onFinishedRef = useRef(onFinished);
  initialRef.current = initialPaths;
  initialFolderRef.current = initialFolder;
  viewRef.current = view;
  onFinishedRef.current = onFinished;

  const complete = useCallback((snapshot: ImportSnapshot) => {
    setProgress(snapshot);
    setSummary(snapshot);
    setView("done");
    if (!finishedRef.current) {
      finishedRef.current = true;
      onFinishedRef.current();
    }
  }, []);

  const addPaths = useCallback((values: string[]) => {
    setPaths((current) => {
      const seen = new Set(current.map((path) => path.toLowerCase()));
      const next = [...current];
      for (const value of values) {
        if (!value || seen.has(value.toLowerCase())) continue;
        seen.add(value.toLowerCase());
        next.push(value);
      }
      return next;
    });
  }, []);

  useEffect(() => {
    if (!open) return;
    setPaths(initialRef.current);
    setFolder(temp ? "" : initialFolderRef.current);
    setFolderMode("flat");
    setFolderPickerOpen(false);
    setTags([]);
    setScore(0);
    setPolicy("skip");
    setOptimizeEnabled(false);
    setOptimizeFormat("jpeg");
    setOptimizeTier("high");
    setView("options");
    setJobId(null);
    setProgress(null);
    setSummary(null);
    setError(null);
    setStarting(false);
    jobIdRef.current = null;
    finishedRef.current = false;
  }, [open, temp]);

  useEffect(() => {
    if (!open) return;
    let disposed = false;
    let unlistenProgress: (() => void) | undefined;
    let unlistenFinished: (() => void) | undefined;

    void (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      if (disposed) return;
      unlistenProgress = await listen<ImportProgress>("import-progress", (event) => {
        if (event.payload.jobId === jobIdRef.current) setProgress(event.payload);
      });
      unlistenFinished = await listen<ImportSnapshot>("import-finished", (event) => {
        if (event.payload.jobId === jobIdRef.current) complete(event.payload);
      });
    })();

    return () => {
      disposed = true;
      unlistenProgress?.();
      unlistenFinished?.();
    };
  }, [open, complete]);

  useEffect(() => {
    if (!open || view !== "running" || !jobId) return;
    let disposed = false;
    const timer = window.setInterval(() => {
      void (async () => {
        try {
          const snapshot = await importApi.status(jobId);
          if (disposed || !snapshot) return;
          setProgress(snapshot);
          if (isTerminal(snapshot.phase)) complete(snapshot);
        } catch {
          // 轮询只是事件丢失时的兜底，错误留给取消/关闭路径处理。
        }
      })();
    }, 500);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [open, view, jobId, complete]);

  useEffect(() => {
    if (!open) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      const { getCurrentWebview } = await import("@tauri-apps/api/webview");
      if (disposed) return;
      unlisten = await getCurrentWebview().onDragDropEvent((event) => {
        if (
          viewRef.current === "options" &&
          event.payload.type === "drop" &&
          event.payload.paths.length > 0
        ) {
          addPaths(event.payload.paths);
        }
      });
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [open, addPaths]);

  const pickFiles = async () => {
    setError(null);
    try {
      const selection = await openDialog({
        multiple: true,
        directory: false,
        title: "选择图片",
        filters: [{ name: "图片", extensions: IMAGE_EXTENSIONS }],
      });
      if (typeof selection === "string") addPaths([selection]);
      else if (Array.isArray(selection)) addPaths(selection);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const pickFolder = async () => {
    setError(null);
    try {
      const selection = await openDialog({
        multiple: false,
        directory: true,
        title: "选择图片目录",
      });
      if (typeof selection === "string") addPaths([selection]);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const start = async () => {
    if (paths.length === 0) {
      setError("请先选择图片或目录");
      return;
    }
    const pendingTags = tagInputRef.current?.commitPending() ?? tags;
    if (!temp && optimizeEnabled) {
      const accepted = await confirmImportOptimize(confirm, paths.length);
      if (!accepted) return;
    }
    setStarting(true);
    setError(null);
    try {
      const snapshot = await importApi.start({
        paths,
        target,
        folder: temp ? "" : folder.trim(),
        folderMode: temp ? "flat" : folderMode,
        tags: pendingTags,
        score,
        duplicatePolicy: temp ? "skip" : policy,
        perceptualThreshold: 6,
        optimize:
          !temp && optimizeEnabled
            ? { tier: optimizeTier, outputFormat: optimizeFormat }
            : null,
      });
      jobIdRef.current = snapshot.jobId;
      setJobId(snapshot.jobId);
      setProgress(snapshot);
      setView("running");
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setStarting(false);
    }
  };

  const cancel = async () => {
    if (!jobId) return;
    try {
      await importApi.cancel(jobId);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const handleClose = () => {
    if (view === "running") return;
    onClose();
  };

  const percent =
    progress && progress.total > 0
      ? Math.min(100, Math.round((progress.processed / progress.total) * 100))
      : 0;

  const footer =
    view === "options" ? (
      <>
        <Button variant="ghost" onClick={handleClose}>
          取消
        </Button>
        <Button
          variant="primary"
          icon={<UploadCloud size={15} />}
          loading={starting}
          disabled={paths.length === 0}
          onClick={() => void start()}
        >
          开始导入
        </Button>
      </>
    ) : view === "running" ? (
      <Button variant="secondary" onClick={() => void cancel()}>
        取消导入
      </Button>
    ) : (
      <Button variant="primary" onClick={handleClose}>
        完成
      </Button>
    );

  return (
    <Modal
      open={open}
      title={view === "options" ? "导入图片" : phaseLabel(progress?.phase ?? "processing")}
      subtitle={
        view === "options"
          ? temp
            ? "原图会复制到临时文件夹，关闭、切换图库或退出应用时清空。"
            : "原图会复制到图库，源文件不动。"
          : view === "running"
            ? "任务在后台执行，可以继续使用其他功能。"
            : undefined
      }
      width={660}
      onClose={handleClose}
      dismissible={view !== "running"}
      footer={footer}
    >
      {view === "options" ? (
        <div className="dialog-form">
          <div className="import-dropzone">
            <span className="import-dropzone-icon">
              <ImagePlus size={22} />
            </span>
            <div className="import-dropzone-text">
              <strong>
                {paths.length === 0 ? "拖入图片或文件夹" : `已选择 ${paths.length} 个路径`}
              </strong>
              <span>支持 JPEG、PNG、BMP、WebP、TIFF、TGA、GIF（首帧）</span>
              {paths.length > 0 ? (
                <ul className="import-path-list">
                  {paths.slice(0, 5).map((path) => (
                    <li key={path} title={path}>
                      {basename(path)}
                    </li>
                  ))}
                  {paths.length > 5 ? <li>还有 {paths.length - 5} 个…</li> : null}
                </ul>
              ) : null}
            </div>
            <div className="import-dropzone-actions">
              <Button size="sm" icon={<ImagePlus size={14} />} onClick={() => void pickFiles()}>
                选择图片
              </Button>
              <Button size="sm" icon={<FolderOpen size={14} />} onClick={() => void pickFolder()}>
                选择文件夹
              </Button>
              {paths.length > 0 ? (
                <Button size="sm" variant="ghost" onClick={() => setPaths([])}>
                  清空
                </Button>
              ) : null}
            </div>
          </div>

          <div className="import-options-grid">
            {!temp ? (
              <Field label="目标文件夹" hint="图库内相对路径，留空表示根目录">
                <div className="input-row">
                  <input
                    className="input"
                    value={folder}
                    placeholder="例如 人物/速写"
                    onChange={(event) => setFolder(event.target.value)}
                  />
                  <Button
                    size="sm"
                    icon={<FolderOpen size={14} />}
                    onClick={() => setFolderPickerOpen(true)}
                  >
                    选择
                  </Button>
                </div>
              </Field>
            ) : null}
            <Field label="初始评分">
              <select
                className="input"
                value={score}
                onChange={(event) => setScore(Number(event.target.value))}
              >
                {[0, 1, 2, 3, 4, 5].map((value) => (
                  <option key={value} value={value}>
                    {value === 0 ? "未评分" : `${value} 星`}
                  </option>
                ))}
              </select>
            </Field>
          </div>

          <Field label="初始标签" hint="最多 32 个">
            <TagInput
              ref={tagInputRef}
              value={tags}
              onChange={setTags}
              max={32}
              placeholder="输入标签，回车添加"
              searchSuggestions={searchTags}
              commitOnBlur
            />
          </Field>

          {!temp ? (
            <div className="field">
              <span className="field-label">目录层级</span>
              <div className="import-policy">
                <button
                  type="button"
                  className={`import-policy-option${folderMode === "flat" ? " is-active" : ""}`}
                  onClick={() => setFolderMode("flat")}
                >
                  <strong>平铺</strong>
                  <span>所有图片进入目标文件夹</span>
                </button>
                <button
                  type="button"
                  className={`import-policy-option${folderMode === "preserve" ? " is-active" : ""}`}
                  onClick={() => setFolderMode("preserve")}
                >
                  <strong>保留源目录</strong>
                  <span>按每个输入目录下的相对层级建文件夹</span>
                </button>
              </div>
            </div>
          ) : null}

          <div className="field">
            <span className="field-label">重复内容</span>
            <div className="import-policy">
              <button
                type="button"
                className={`import-policy-option${temp || policy === "skip" ? " is-active" : ""}`}
                onClick={() => setPolicy("skip")}
              >
                <strong>跳过</strong>
                <span>内容相同的只保留一张</span>
              </button>
              {!temp ? (
                <button
                  type="button"
                  className={`import-policy-option${policy === "merge" ? " is-active" : ""}`}
                  onClick={() => setPolicy("merge")}
                >
                  <strong>合并标签与评分</strong>
                  <span>标签取并集，评分取较高值</span>
                </button>
              ) : null}
            </div>
          </div>

          {!temp ? (
            <div className="field">
              <label className="toggle-row">
                <input
                  type="checkbox"
                  checked={optimizeEnabled}
                  onChange={(event) => setOptimizeEnabled(event.target.checked)}
                />
                <span>
                  <strong>导入时压缩</strong>
                  <small>原图不会保留，无法还原。</small>
                </span>
              </label>
              {optimizeEnabled ? (
                <div className="import-optimize">
                  <Field
                    label="输出格式"
                    hint={
                      optimizeFormat === "webp"
                        ? "WebP 不保留 EXIF 与色彩配置；超过 16383 px 的图会保留原图。"
                        : "透明图会保留原图。"
                    }
                  >
                    <div className="optimize-choice-row">
                      <button
                        type="button"
                        className={`optimize-choice${optimizeFormat === "jpeg" ? " is-active" : ""}`}
                        onClick={() => setOptimizeFormat("jpeg")}
                      >
                        JPG
                      </button>
                      <button
                        type="button"
                        className={`optimize-choice${optimizeFormat === "webp" ? " is-active" : ""}`}
                        onClick={() => setOptimizeFormat("webp")}
                      >
                        WebP
                      </button>
                    </div>
                  </Field>
                  <div className="field">
                    <span className="field-label">质量档位</span>
                    <div className="optimize-choice-row">
                      {OPTIMIZE_TIERS.map((item) => (
                        <button
                          key={item.value}
                          type="button"
                          className={`optimize-choice${optimizeTier === item.value ? " is-active" : ""}`}
                          onClick={() => setOptimizeTier(item.value)}
                        >
                          {item.label}
                        </button>
                      ))}
                    </div>
                  </div>
                  <span className="field-hint">
                    压不动的图保留原图；同一源图用同样参数重复导入仍然去重，改档位或格式会变成两条记录。
                  </span>
                </div>
              ) : null}
            </div>
          ) : null}

          {error ? (
            <div className="import-error">
              <AlertTriangle size={15} />
              <span>{error}</span>
            </div>
          ) : null}

          <FolderPickerDialog
            open={folderPickerOpen}
            title="选择目标文件夹"
            subtitle="图片会进入该文件夹；保留源目录时在其下继续建子文件夹。"
            folders={folders}
            selectedPath={folder}
            onClose={() => setFolderPickerOpen(false)}
            onSelect={(target) => {
              setFolder(target.path);
              setFolderPickerOpen(false);
            }}
          />
        </div>
      ) : (
        <div className="import-progress">
          {view === "running" ? (
            <div className="import-progress-heading">
              <div>
                <strong>{phaseLabel(progress?.phase ?? "processing")}</strong>
                <span>{progress?.current ? basename(progress.current) : "准备中…"}</span>
              </div>
              <em>{percent}%</em>
            </div>
          ) : null}

          <div className="import-progress-track" role="progressbar" aria-valuenow={percent}>
            <div
              className={`import-progress-fill${progress?.phase === "cancelled" ? " is-cancelled" : ""}`}
              style={{ width: `${percent}%` }}
            />
          </div>

          <div className="import-stat-row">
            <div>
              <strong>{progress?.imported ?? 0}</strong>
              <span>新增</span>
            </div>
            <div>
              <strong>{progress?.duplicates ?? 0}</strong>
              <span>重复跳过</span>
            </div>
            <div>
              <strong>{progress?.merged ?? 0}</strong>
              <span>合并</span>
            </div>
            <div>
              <strong>{progress?.failed ?? 0}</strong>
              <span>失败</span>
            </div>
            <div>
              <strong>{progress?.similar ?? 0}</strong>
              <span>疑似重复</span>
            </div>
          </div>

          {!temp && summary && (summary.optimized > 0 || optimizeEnabled) ? (
            <div className="import-note">
              {summary.optimized > 0
                ? `其中 ${summary.optimized} 张已压缩，省 ${formatBytes(summary.optimizedBytes)}。`
                : "没有图片被压缩：都已足够紧凑，或不满足压缩条件。"}
            </div>
          ) : null}

          {temp && view !== "running" ? (
            <div className="import-note">
              临时图片会在关闭、切换图库或退出应用时清空。
            </div>
          ) : null}

          <div className="import-progress-meta">
            <span>
              {progress?.processed ?? 0} / {progress?.total ?? 0} 个文件
            </span>
            <span>{formatDuration(progress?.elapsedMs ?? 0)}</span>
          </div>

          {summary && summary.previewIds.length > 0 ? (
            <div className="import-preview">
              <div className="import-preview-heading">
                <Images size={15} />
                <span>最近导入</span>
              </div>
              <div className="import-preview-grid">
                {summary.previewIds.map((id) => (
                  <img key={id} src={thumbnailUrl(id)} alt="" loading="lazy" />
                ))}
              </div>
            </div>
          ) : null}

          {summary && summary.failed > 0 ? (
            <details className="import-failures">
              <summary>失败 {summary.failed} 项（最多显示 200 条）</summary>
              <ul>
                {summary.failures.map((failure, index) => (
                  <li key={`${failure.path}-${index}`}>
                    <span title={failure.path}>{failure.path || "导入任务"}</span>
                    <em>{failure.reason}</em>
                  </li>
                ))}
              </ul>
            </details>
          ) : null}

          {summary && summary.similar > 0 ? (
            <details className="import-failures">
              <summary>疑似重复 {summary.similar} 项（最多显示 100 条）</summary>
              <ul>
                {summary.hints.map((hint) => (
                  <li key={`${hint.imageId}-${hint.similarTo}`}>
                    <span>图片 #{hint.imageId}</span>
                    <em>与 #{hint.similarTo} 相似</em>
                  </li>
                ))}
              </ul>
            </details>
          ) : null}

          {summary?.phase === "cancelled" ? (
            <p className="import-note">已取消：已入库的图片保留。</p>
          ) : null}
          {summary?.phase === "failed" ? (
            <p className="import-note import-note-danger">任务异常结束，请查看失败清单。</p>
          ) : null}
          {error ? (
            <div className="import-error">
              <AlertTriangle size={15} />
              <span>{error}</span>
            </div>
          ) : null}
        </div>
      )}
    </Modal>
  );
}

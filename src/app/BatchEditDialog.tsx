import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import {
  AlertCircle,
  CheckCircle2,
  FlipHorizontal2,
  FlipVertical2,
  Play,
  RotateCcw,
  RotateCw,
  Square,
} from "lucide-react";
import { Button } from "../shared/components/Button";
import { Field } from "../shared/components/Field";
import { Modal } from "../shared/components/Modal";
import { Spinner } from "../shared/components/Spinner";
import type {
  BatchEditOptions,
  BatchFit,
  BatchOutputFormat,
  CropRect,
} from "../shared/contracts/edit";
import {
  MAX_EDIT_DIMENSION,
  MAX_EDIT_PIXELS,
  clamp,
  clampCrop,
  defaultQualityFor,
  formatBytes,
  formatDimensions,
  rectBetween,
  transformedSize,
} from "../shared/editGeometry";
import { errorMessage } from "../shared/errors";
import { thumbnailUrl } from "../shared/thumbUrl";
import { libraryApi } from "../modules/library/api";
import type { ImageDetails } from "../modules/library/types";
import { useTaskCenter } from "./taskContext";
import { taskApi } from "./taskApi";
import { CancelPendingNote, TaskProgressBar } from "./taskProgress";
import {
  TASK_PHASE_LABELS,
  isTaskActive,
  taskPercent,
  type BatchTaskPreview,
} from "./taskTypes";

const SAMPLE_COUNT = 9;
const PREVIEW_DEBOUNCE_MS = 250;
const REVERSIBLE_LIMIT = 2000;

const SKIP_LABELS: Record<string, string> = {
  missing: "原图缺失",
  trashed: "在回收站",
  temp: "临时图片",
  animated: "动画 GIF",
  oversize: "超过 50MP",
  unchanged: "无需修改",
};

const FORMAT_LABELS: Record<BatchOutputFormat, string> = {
  keep: "保持原格式",
  jpeg: "JPEG",
  png: "PNG",
  webp: "WebP",
};

interface BatchEditDialogProps {
  open: boolean;
  imageIds: number[];
  libraryId: string;
  onClose: () => void;
}

function pickSamples(ids: number[], count: number): number[] {
  if (ids.length <= count) return ids;
  const picked: number[] = [];
  for (let index = 0; index < count; index += 1) {
    picked.push(ids[Math.round((index * (ids.length - 1)) / (count - 1))]);
  }
  return Array.from(new Set(picked));
}

function qualityLabel(
  format: Exclude<BatchOutputFormat, "keep">,
  quality: number | null,
): string {
  if (format === "png") return "无损";
  return `质量 ${quality ?? defaultQualityFor(format)}`;
}

/** 图层与缩略图的 CSS 百分比：图层按 `frame` 归一，缩略图按图层归一。 */
function layerLayout({
  width,
  height,
  rotate,
  flipHorizontal,
  flipVertical,
  mode,
  fit,
  canvasWidth,
  canvasHeight,
  crop,
}: {
  width: number;
  height: number;
  rotate: number;
  flipHorizontal: boolean;
  flipVertical: boolean;
  mode: BatchEditOptions["mode"];
  fit: BatchFit;
  canvasWidth: number;
  canvasHeight: number;
  /** 画布坐标里的裁剪框；为空时输出框就是整块画布。 */
  crop: CropRect | null;
}) {
  const transformed = transformedSize(width, height, rotate);
  const box =
    mode === "canvas"
      ? { width: canvasWidth, height: canvasHeight }
      : { width: transformed.width, height: transformed.height };
  const scale =
    mode === "canvas"
      ? fit === "cover"
        ? Math.max(box.width / transformed.width, box.height / transformed.height)
        : Math.min(box.width / transformed.width, box.height / transformed.height)
      : 1;
  const displayWidth = transformed.width * scale;
  const displayHeight = transformed.height * scale;
  const offsetX = (box.width - displayWidth) / 2;
  const offsetY = (box.height - displayHeight) / 2;
  const sourceWidth = width * scale;
  const sourceHeight = height * scale;
  const frame = crop ?? { x: 0, y: 0, width: box.width, height: box.height };
  const percent = (value: number) => `${value * 100}%`;
  return {
    box,
    frame,
    layer: {
      left: percent((offsetX - frame.x) / frame.width),
      top: percent((offsetY - frame.y) / frame.height),
      width: percent(displayWidth / frame.width),
      height: percent(displayHeight / frame.height),
    },
    image: {
      left: percent((displayWidth - sourceWidth) / 2 / displayWidth),
      top: percent((displayHeight - sourceHeight) / 2 / displayHeight),
      width: percent(sourceWidth / displayWidth),
      height: percent(sourceHeight / displayHeight),
      transform: `rotate(${rotate * 90}deg) scaleX(${flipHorizontal ? -1 : 1}) scaleY(${
        flipVertical ? -1 : 1
      })`,
    },
  };
}

/** 单张预览：按画布/原图坐标把缩略图摆到最终裁剪框里。 */
function PreviewTile({
  detail,
  rotate,
  flipHorizontal,
  flipVertical,
  mode,
  fit,
  canvasWidth,
  canvasHeight,
  crop,
  reference,
}: {
  detail: ImageDetails;
  rotate: number;
  flipHorizontal: boolean;
  flipVertical: boolean;
  mode: BatchEditOptions["mode"];
  fit: BatchFit;
  canvasWidth: number;
  canvasHeight: number;
  crop: CropRect | null;
  reference: boolean;
}) {
  const layout = layerLayout({
    width: detail.width,
    height: detail.height,
    rotate,
    flipHorizontal,
    flipVertical,
    mode,
    fit,
    canvasWidth,
    canvasHeight,
    crop,
  });
  return (
    <div
      className={`batch-edit-tile${reference ? " is-reference" : ""}`}
      style={{ aspectRatio: `${layout.frame.width} / ${layout.frame.height}` }}
    >
      <div className="batch-edit-layer" style={layout.layer}>
        <img
          src={thumbnailUrl(detail.id, detail.thumbVersion)}
          alt=""
          draggable={false}
          style={layout.image}
        />
      </div>
    </div>
  );
}

export function BatchEditDialog({
  open,
  imageIds,
  libraryId,
  onClose,
}: BatchEditDialogProps) {
  const idsKey = imageIds.join(",");
  // 选择变化时父组件会传新数组；按 idsKey 固定引用，避免每次渲染都重跑取详情与预检。
  const selectedIds = useMemo(() => imageIds, [idsKey]);
  const sampleIds = useMemo(() => pickSamples(selectedIds, SAMPLE_COUNT), [selectedIds]);

  const [details, setDetails] = useState<ImageDetails[]>([]);
  const [referenceId, setReferenceId] = useState<number | null>(null);
  const [rotate, setRotate] = useState(0);
  const [flipHorizontal, setFlipHorizontal] = useState(false);
  const [flipVertical, setFlipVertical] = useState(false);
  const [mode, setMode] = useState<BatchEditOptions["mode"]>("keepSize");
  const [canvasWidth, setCanvasWidth] = useState(0);
  const [canvasHeight, setCanvasHeight] = useState(0);
  const [canvasTouched, setCanvasTouched] = useState(false);
  const [fit, setFit] = useState<BatchFit>("contain");
  const [crop, setCrop] = useState<CropRect | null>(null);
  const [outputFormat, setOutputFormat] = useState<BatchOutputFormat>("keep");
  const [quality, setQuality] = useState<number | null>(null);
  const [preview, setPreview] = useState<BatchTaskPreview | null>(null);
  const [previewStatus, setPreviewStatus] = useState<"idle" | "loading" | "ready" | "error">(
    "idle",
  );
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [taskId, setTaskId] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setDetails([]);
    setReferenceId(selectedIds[0] ?? null);
    setRotate(0);
    setFlipHorizontal(false);
    setFlipVertical(false);
    setMode("keepSize");
    setCanvasWidth(0);
    setCanvasHeight(0);
    setCanvasTouched(false);
    setFit("contain");
    setCrop(null);
    setOutputFormat("keep");
    setQuality(null);
    setPreview(null);
    setPreviewStatus("idle");
    setStarting(false);
    setError(null);
    setTaskId(null);
    // 打开时重置；运行中的任务不会因为选项变化被清掉。
  }, [open, selectedIds, libraryId]);

  useEffect(() => {
    if (!open || sampleIds.length === 0) return;
    let alive = true;
    void Promise.all(sampleIds.map((id) => libraryApi.getImageDetail(id)))
      .then((rows) => {
        if (!alive) return;
        setDetails(rows);
      })
      .catch((cause) => {
        if (alive) setError(errorMessage(cause));
      });
    return () => {
      alive = false;
    };
  }, [open, sampleIds]);

  const reference = details.find((item) => item.id === referenceId) ?? details[0] ?? null;
  const referenceSize = reference
    ? transformedSize(reference.width, reference.height, rotate)
    : null;

  useEffect(() => {
    if (canvasTouched || !referenceSize) return;
    setCanvasWidth(referenceSize.width);
    setCanvasHeight(referenceSize.height);
  }, [canvasTouched, referenceSize?.width, referenceSize?.height]);

  useEffect(() => {
    setCrop(null);
  }, [referenceId, mode, rotate]);

  const { start, find, cancel } = useTaskCenter();
  const task = taskId ? find(taskId) : null;
  const activeTask = task && isTaskActive(task) ? task : null;
  const cancelling = task?.phase === "cancelling";
  const percent = task ? taskPercent(task) : 0;

  const options = useMemo<BatchEditOptions>(
    () => ({
      rotate,
      flipHorizontal,
      flipVertical,
      mode,
      canvas: { width: canvasWidth, height: canvasHeight, fit },
      crop: mode === "canvas" ? crop : null,
      outputFormat,
      quality,
      reversible: true,
    }),
    [
      rotate,
      flipHorizontal,
      flipVertical,
      mode,
      canvasWidth,
      canvasHeight,
      fit,
      crop,
      outputFormat,
      quality,
    ],
  );

  const canvasValid =
    mode === "keepSize" ||
    (canvasWidth >= 1 &&
      canvasHeight >= 1 &&
      canvasWidth <= MAX_EDIT_DIMENSION &&
      canvasHeight <= MAX_EDIT_DIMENSION &&
      canvasWidth * canvasHeight <= MAX_EDIT_PIXELS &&
      referenceSize != null);

  useEffect(() => {
    if (!open || activeTask || !canvasValid || selectedIds.length === 0) return;
    setPreview(null);
    setPreviewStatus("loading");
    let cancelled = false;
    const timer = window.setTimeout(() => {
      setError(null);
      void taskApi
        .previewBatch({
          kind: "edit",
          scope: { type: "selected", imageIds: selectedIds },
          overwrite: false,
          capability: null,
          edit: options,
        })
        .then((result) => {
          if (cancelled) return;
          setPreview(result);
          setPreviewStatus("ready");
        })
        .catch((cause) => {
          if (cancelled) return;
          setPreviewStatus("error");
          setError(errorMessage(cause));
        });
    }, PREVIEW_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [open, activeTask, canvasValid, selectedIds, options]);

  const outputSize =
    mode === "canvas"
      ? crop ?? { width: canvasWidth, height: canvasHeight }
      : null;

  const totalCount = preview?.total ?? selectedIds.length;
  const willProcess = preview?.willProcess ?? 0;
  const tooMany = totalCount > REVERSIBLE_LIMIT;
  const summaryTone =
    previewStatus === "error" ||
    !canvasValid ||
    tooMany ||
    (previewStatus === "ready" && willProcess === 0)
      ? "danger"
      : previewStatus === "ready"
        ? "success"
        : "pending";
  const summaryStatus = !canvasValid
    ? "画布尺寸无效"
    : previewStatus === "error"
      ? "预检失败"
      : tooMany
        ? `超过可撤销上限（${totalCount.toLocaleString()} 张）`
        : previewStatus === "ready"
          ? willProcess === 0
            ? "没有可处理的图片"
            : `预检通过 · 可处理 ${willProcess.toLocaleString()} / ${totalCount.toLocaleString()} 张`
          : "正在预检…";
  const canStart =
    !starting &&
    canvasValid &&
    previewStatus === "ready" &&
    willProcess > 0 &&
    !activeTask;

  const launch = async (reversible: boolean) => {
    if (!canStart) return;
    setStarting(true);
    setError(null);
    try {
      const recipe: BatchEditOptions = { ...options, reversible };
      const result = await start({
        kind: "edit",
        scope: { type: "selected", imageIds: selectedIds },
        overwrite: false,
        recalcVisuals: false,
        prompt: null,
        maxTokens: null,
        edit: recipe,
        conflict: "queue",
      });
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
  ) : tooMany ? (
    <>
      <Button variant="ghost" onClick={onClose}>
        返回并缩小选择（推荐）
      </Button>
      <Button variant="danger" loading={starting} onClick={() => void launch(false)}>
        继续，本次不可撤销
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
        disabled={!canStart}
        onClick={() => void launch(true)}
      >
        开始编辑
      </Button>
    </>
  );

  const qualityValue =
    outputFormat === "jpeg" || outputFormat === "webp"
      ? quality ?? defaultQualityFor(outputFormat)
      : null;
  const showQuality = qualityValue != null;

  return (
    <Modal
      open={open}
      title="批量编辑"
      subtitle={`对选中的 ${selectedIds.length.toLocaleString()} 张图片应用同一套设置。`}
      width={1040}
      height={760}
      onClose={onClose}
      footer={footer}
    >
      <div className="batch-edit">
        <div className="batch-edit-layout">
          <div className="batch-edit-preview-pane">
            <div className="batch-edit-canvas-wrap">
              {reference && referenceSize && canvasValid ? (
                <CropStage
                  detail={reference}
                  rotate={rotate}
                  flipHorizontal={flipHorizontal}
                  flipVertical={flipVertical}
                  mode={mode}
                  fit={fit}
                  canvasWidth={mode === "canvas" ? canvasWidth : referenceSize.width}
                  canvasHeight={mode === "canvas" ? canvasHeight : referenceSize.height}
                  crop={mode === "canvas" ? crop : null}
                  interactive={mode === "canvas"}
                  onCrop={setCrop}
                />
              ) : reference && referenceSize ? (
                <div className="batch-edit-canvas-empty">
                  <span>画布尺寸无效，请调整宽高</span>
                </div>
              ) : (
                <div className="batch-edit-canvas-empty">
                  <Spinner size={16} />
                  <span>正在读取参考图…</span>
                </div>
              )}
            </div>

            <section className="batch-edit-preview-section">
              <div className="batch-edit-heading">
                <h4>效果预览</h4>
                <span className="field-hint">点缩略图切换参考图</span>
              </div>
              <div className="batch-edit-previews">
                {details.map((item) => {
                  const active = item.id === reference?.id;
                  return (
                    <button
                      key={item.id}
                      type="button"
                      className="batch-edit-preview-button"
                      aria-pressed={active}
                      onClick={() => {
                        setReferenceId(item.id);
                        setCanvasTouched(false);
                      }}
                    >
                      <PreviewTile
                        detail={item}
                        rotate={rotate}
                        flipHorizontal={flipHorizontal}
                        flipVertical={flipVertical}
                        mode={mode}
                        fit={fit}
                        canvasWidth={canvasWidth}
                        canvasHeight={canvasHeight}
                        crop={mode === "canvas" ? crop : null}
                        reference={active}
                      />
                      {active ? <span className="batch-edit-reference-badge">参考</span> : null}
                    </button>
                  );
                })}
              </div>
            </section>
          </div>

          <div className="batch-edit-controls">
            <section className="batch-edit-section">
              <h4>方向</h4>
              <div className="batch-edit-direction">
                <Button
                  size="sm"
                  icon={<RotateCcw size={13} />}
                  onClick={() => setRotate((value) => (value + 3) % 4)}
                >
                  左转
                </Button>
                <Button
                  size="sm"
                  icon={<RotateCw size={13} />}
                  onClick={() => setRotate((value) => (value + 1) % 4)}
                >
                  右转
                </Button>
                <Button
                  size="sm"
                  variant={flipHorizontal ? "primary" : "secondary"}
                  icon={<FlipHorizontal2 size={13} />}
                  aria-pressed={flipHorizontal}
                  onClick={() => setFlipHorizontal((value) => !value)}
                >
                  水平翻转
                </Button>
                <Button
                  size="sm"
                  variant={flipVertical ? "primary" : "secondary"}
                  icon={<FlipVertical2 size={13} />}
                  aria-pressed={flipVertical}
                  onClick={() => setFlipVertical((value) => !value)}
                >
                  垂直翻转
                </Button>
              </div>
            </section>

            <section className="batch-edit-section">
              <h4>尺寸</h4>
              <div className="batch-edit-mode">
                <label
                  className={`batch-edit-mode-option${mode === "keepSize" ? " is-active" : ""}`}
                >
                  <input
                    type="radio"
                    checked={mode === "keepSize"}
                    onChange={() => {
                      setMode("keepSize");
                      setCrop(null);
                    }}
                  />
                  <span>
                    <strong>保持各自原尺寸</strong>
                    <small>只旋转或翻转，不缩放、不裁剪。</small>
                  </span>
                </label>
                <label
                  className={`batch-edit-mode-option${mode === "canvas" ? " is-active" : ""}`}
                >
                  <input
                    type="radio"
                    checked={mode === "canvas"}
                    onChange={() => setMode("canvas")}
                  />
                  <span>
                    <strong>统一尺寸裁剪</strong>
                    <small>所有图片缩放到同一块画布，可再框选裁剪。</small>
                  </span>
                </label>
              </div>

              {mode === "canvas" ? (
                <div className="batch-edit-canvas-options">
                  <div className="batch-edit-size-row">
                    <Field label="宽">
                      <input
                        className="input"
                        type="number"
                        min={1}
                        max={MAX_EDIT_DIMENSION}
                        value={canvasWidth || ""}
                        onChange={(event) => {
                          setCanvasTouched(true);
                          setCanvasWidth(Math.max(1, Math.round(Number(event.target.value) || 0)));
                        }}
                      />
                    </Field>
                    <Field label="高">
                      <input
                        className="input"
                        type="number"
                        min={1}
                        max={MAX_EDIT_DIMENSION}
                        value={canvasHeight || ""}
                        onChange={(event) => {
                          setCanvasTouched(true);
                          setCanvasHeight(Math.max(1, Math.round(Number(event.target.value) || 0)));
                        }}
                      />
                    </Field>
                  </div>
                  <Field label="不同比例时">
                    <select
                      className="input"
                      value={fit}
                      onChange={(event) => setFit(event.target.value as BatchFit)}
                    >
                      <option value="contain">完整显示（可能补边）</option>
                      <option value="cover">填满画布（会裁边）</option>
                    </select>
                  </Field>
                  <div className="batch-edit-actions">
                    <Button
                      size="sm"
                      disabled={!referenceSize}
                      onClick={() => {
                        if (!referenceSize) return;
                        setCanvasTouched(false);
                        setCanvasWidth(referenceSize.width);
                        setCanvasHeight(referenceSize.height);
                      }}
                    >
                      使用参考图尺寸
                    </Button>
                    {crop ? (
                      <Button size="sm" variant="ghost" onClick={() => setCrop(null)}>
                        清除裁剪
                      </Button>
                    ) : null}
                  </div>
                  <span className="field-hint">
                    {crop
                      ? `裁剪输出 ${formatDimensions(crop.width, crop.height)}；在左侧拖动可重新框选。`
                      : `输出 ${formatDimensions(canvasWidth, canvasHeight)}；在左侧拖动可选择裁剪区域。`}
                  </span>
                </div>
              ) : (
                <span className="field-hint">旋转或翻转后，每张图片保留自己的尺寸。</span>
              )}
            </section>

            <section className="batch-edit-section">
              <h4>输出</h4>
              <div className={`batch-edit-output-row${showQuality ? "" : " is-single"}`}>
                <Field label="格式">
                  <select
                    className="input"
                    value={outputFormat}
                    onChange={(event) => {
                      setOutputFormat(event.target.value as BatchOutputFormat);
                      // 各格式沿用各自的默认质量；换格式时回到该格式的默认值。
                      setQuality(null);
                    }}
                  >
                    {(Object.keys(FORMAT_LABELS) as BatchOutputFormat[]).map((value) => (
                      <option key={value} value={value}>
                        {FORMAT_LABELS[value]}
                      </option>
                    ))}
                  </select>
                </Field>
                {showQuality ? (
                  <Field label="质量" plain>
                    <div className="batch-edit-quality-row">
                      <input
                        className="batch-edit-quality"
                        type="range"
                        min={1}
                        max={100}
                        value={qualityValue ?? 90}
                        aria-label="输出质量"
                        onChange={(event) => setQuality(Number(event.target.value))}
                      />
                      <span className="batch-edit-quality-value">{qualityValue}</span>
                    </div>
                  </Field>
                ) : null}
              </div>
              {outputFormat === "keep" ? (
                <span className="field-hint">
                  保持原格式时用各格式默认质量：JPEG 90、WebP 85、PNG 无损。
                </span>
              ) : null}
              {outputFormat === "png" ? (
                <span className="field-hint">PNG 无损，不调整质量。</span>
              ) : null}
              {outputFormat === "jpeg" && reference?.hasAlpha ? (
                <span className="field-hint">JPEG 不支持透明，透明区域会合成到白色背景。</span>
              ) : null}
            </section>
          </div>
        </div>

        <section className={`batch-edit-summary is-${summaryTone}`}>
          <div className="batch-edit-summary-head">
            <span className="batch-edit-summary-icon" aria-hidden="true">
              {summaryTone === "success" ? (
                <CheckCircle2 size={17} />
              ) : summaryTone === "danger" ? (
                <AlertCircle size={17} />
              ) : (
                <Spinner size={14} />
              )}
            </span>
            <div className="batch-edit-summary-heading">
              <strong>确认</strong>
              <span aria-live="polite">{summaryStatus}</span>
            </div>
          </div>
          <dl>
            <div>
              <dt>输出</dt>
              <dd>
                {outputSize
                  ? formatDimensions(outputSize.width, outputSize.height)
                  : "各自原尺寸"}{" "}
                · {FORMAT_LABELS[outputFormat]}
                {outputFormat !== "keep"
                  ? ` · ${qualityLabel(outputFormat, quality)}`
                  : ""}
              </dd>
            </div>
            <div>
              <dt>预计新增</dt>
              <dd>
                {preview?.estimatedOutputBytes != null
                  ? formatBytes(preview.estimatedOutputBytes)
                  : "—"}
                {preview?.availableBytes != null
                  ? `（磁盘可用 ${formatBytes(preview.availableBytes)}）`
                  : ""}
              </dd>
            </div>
            <div>
              <dt>图片特征</dt>
              <dd>编辑后直接丢弃，需要在批量任务里重新生成。</dd>
            </div>
            <div>
              <dt>画质</dt>
              <dd>重新编码会损失画质，原 EXIF/ICC 不保留。</dd>
            </div>
            {preview?.skipBreakdown?.length ? (
              <div>
                <dt>跳过</dt>
                <dd>
                  {preview.skipBreakdown
                    .filter((bucket) => bucket.count > 0)
                    .map((bucket) => `${SKIP_LABELS[bucket.key] ?? bucket.key} ${bucket.count}`)
                    .join(" · ")}
                </dd>
              </div>
            ) : null}
          </dl>
          {tooMany ? (
            <p className="field-hint">
              超过 {REVERSIBLE_LIMIT} 张时只能分批处理，或继续但本次不保留撤销副本。
            </p>
          ) : null}
        </section>

        {task ? (
          <section className="batch-edit-progress">
            <div className="batch-edit-heading">
              <strong>
                {task.label} · {TASK_PHASE_LABELS[task.phase]}
              </strong>
              <span>
                {(task.completed + task.failed + task.skipped).toLocaleString()} /{" "}
                {task.total.toLocaleString()}
              </span>
            </div>
            <TaskProgressBar percent={percent} cancelling={cancelling} />
            <CancelPendingNote visible={cancelling} />
            <div className="batch-edit-heading">
              <span>{task.message || "正在处理…"}</span>
              {task.failed > 0 ? <span>失败 {task.failed.toLocaleString()} 张</span> : null}
            </div>
          </section>
        ) : null}

        {error ? <div className="form-error">{error}</div> : null}
      </div>
    </Modal>
  );
}

/** 参考画布上的裁剪交互；坐标直接落在画布/原图像素空间。 */
function CropStage({
  detail,
  rotate,
  flipHorizontal,
  flipVertical,
  mode,
  fit,
  canvasWidth,
  canvasHeight,
  crop,
  interactive,
  onCrop,
}: {
  detail: ImageDetails;
  rotate: number;
  flipHorizontal: boolean;
  flipVertical: boolean;
  mode: BatchEditOptions["mode"];
  fit: BatchFit;
  canvasWidth: number;
  canvasHeight: number;
  crop: CropRect | null;
  interactive: boolean;
  onCrop: (crop: CropRect | null) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const start = useRef<{ x: number; y: number } | null>(null);
  const [draft, setDraft] = useState<CropRect | null>(null);

  // 舞台按整块画布摆放；裁剪框只作为叠加层，不参与图层归一。
  const layout = layerLayout({
    width: detail.width,
    height: detail.height,
    rotate,
    flipHorizontal,
    flipVertical,
    mode,
    fit,
    canvasWidth,
    canvasHeight,
    crop: null,
  });
  const box = layout.box;

  const point = (event: ReactPointerEvent<HTMLDivElement>) => {
    const rect = ref.current?.getBoundingClientRect();
    if (!rect || rect.width <= 0 || rect.height <= 0) return null;
    return {
      x: clamp(((event.clientX - rect.left) / rect.width) * box.width, 0, box.width),
      y: clamp(((event.clientY - rect.top) / rect.height) * box.height, 0, box.height),
    };
  };

  const shown = interactive ? (draft ?? crop) : null;
  return (
    <div
      ref={ref}
      className={`batch-edit-canvas${interactive ? " is-interactive" : ""}`}
      style={{
        aspectRatio: `${box.width} / ${box.height}`,
        width: `min(100%, ${Math.round(Math.min(720, (400 * box.width) / box.height))}px)`,
      }}
      onPointerDown={
        interactive
          ? (event) => {
              const next = point(event);
              if (!next) return;
              event.currentTarget.setPointerCapture(event.pointerId);
              start.current = next;
              setDraft(null);
            }
          : undefined
      }
      onPointerMove={
        interactive
          ? (event) => {
              const from = start.current;
              if (!from) return;
              const to = point(event);
              if (!to) return;
              setDraft(rectBetween(from, to, box.width, box.height));
            }
          : undefined
      }
      onPointerUp={
        interactive
          ? (event) => {
              const from = start.current;
              start.current = null;
              if (!from) return;
              const to = point(event);
              if (!to) return;
              // 后端裁剪框是 u32 像素；指针坐标必须取整后再提交。
              const rect = rectBetween(from, to, box.width, box.height);
              onCrop(rect ? clampCrop(rect, box.width, box.height) : null);
              setDraft(null);
            }
          : undefined
      }
      onPointerCancel={
        interactive
          ? () => {
              start.current = null;
              setDraft(null);
            }
          : undefined
      }
    >
      <div className="batch-edit-layer" style={layout.layer}>
        <img
          src={thumbnailUrl(detail.id, detail.thumbVersion)}
          alt=""
          draggable={false}
          style={layout.image}
        />
      </div>
      {shown ? (
        <div
          className="batch-edit-crop"
          style={{
            left: `${(shown.x / box.width) * 100}%`,
            top: `${(shown.y / box.height) * 100}%`,
            width: `${(shown.width / box.width) * 100}%`,
            height: `${(shown.height / box.height) * 100}%`,
          }}
        >
          <span>
            {formatDimensions(Math.round(shown.width), Math.round(shown.height))}
          </span>
        </div>
      ) : null}
    </div>
  );
}

import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { Crop, Play, Square } from "lucide-react";
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
  clamp,
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

function qualityLabel(format: BatchOutputFormat, quality: number | null): string {
  if (format === "png") return "无损";
  if (format === "keep") {
    return quality == null ? "按格式默认" : `统一质量 ${quality}`;
  }
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
  const [mode, setMode] = useState<BatchEditOptions["mode"]>("canvas");
  const [canvasWidth, setCanvasWidth] = useState(0);
  const [canvasHeight, setCanvasHeight] = useState(0);
  const [canvasTouched, setCanvasTouched] = useState(false);
  const [fit, setFit] = useState<BatchFit>("cover");
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
    setMode("canvas");
    setCanvasWidth(0);
    setCanvasHeight(0);
    setCanvasTouched(false);
    setFit("cover");
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

  const firstSize = details[0]
    ? transformedSize(details[0].width, details[0].height, rotate)
    : null;
  const localUniform = details.every((item) => {
    const size = transformedSize(item.width, item.height, rotate);
    return (
      !firstSize || (size.width === firstSize.width && size.height === firstSize.height)
    );
  });
  const samePixelReady = localUniform && (preview?.uniformDimensions ?? true);
  useEffect(() => {
    if (mode === "samePixel" && previewStatus === "ready" && !samePixelReady) {
      setMode("canvas");
    }
  }, [mode, previewStatus, samePixelReady]);

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
      crop,
      outputFormat,
      quality,
      reversible: true,
    }),
    [rotate, flipHorizontal, flipVertical, mode, canvasWidth, canvasHeight, fit, crop, outputFormat, quality],
  );

  const canvasValid =
    canvasWidth >= 1 &&
    canvasHeight >= 1 &&
    canvasWidth <= 65_535 &&
    canvasHeight <= 65_535 &&
    canvasWidth * canvasHeight <= 50_000_000 &&
    (mode === "canvas" || referenceSize != null);

  useEffect(() => {
    if (!open || activeTask || !canvasValid || selectedIds.length === 0) return;
    setPreview(null);
    setPreviewStatus("loading");
    let cancelled = false;
    const timer = window.setTimeout(() => {
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
    crop ??
    (mode === "canvas"
      ? { width: canvasWidth, height: canvasHeight }
      : referenceSize ?? { width: 0, height: 0 });

  const tooMany = (preview?.total ?? selectedIds.length) > REVERSIBLE_LIMIT;
  const canStart =
    !starting &&
    canvasValid &&
    previewStatus === "ready" &&
    (preview?.willProcess ?? 0) > 0 &&
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

  return (
    <Modal
      open={open}
      title="批量编辑"
      subtitle="整批使用同一套旋转、翻转、裁剪与输出设置；文件会重新编码。"
      width={920}
      onClose={onClose}
      footer={footer}
    >
      <div className="batch-edit">
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
              crop={crop}
              onCrop={setCrop}
            />
          ) : (
            <div className="batch-edit-canvas-empty">
              <Spinner size={16} />
              <span>正在读取参考图…</span>
            </div>
          )}
        </div>

        <div className="batch-edit-grid">
          <section className="batch-edit-section">
            <h4>方向</h4>
            <div className="batch-edit-actions">
              <Button size="sm" onClick={() => setRotate((value) => (value + 3) % 4)}>
                左转 90°
              </Button>
              <Button size="sm" onClick={() => setRotate((value) => (value + 1) % 4)}>
                右转 90°
              </Button>
              <Button size="sm" onClick={() => setFlipHorizontal((value) => !value)}>
                水平翻转
              </Button>
              <Button size="sm" onClick={() => setFlipVertical((value) => !value)}>
                垂直翻转
              </Button>
            </div>
          </section>

          <section className="batch-edit-section">
            <h4>画布</h4>
            <div className="batch-edit-mode">
              <label className="toggle-row">
                <input
                  type="radio"
                  checked={mode === "canvas"}
                  onChange={() => setMode("canvas")}
                />
                <span>
                  <strong>统一画布</strong>
                  <small>按画布比例适配，再套同一个裁剪框。</small>
                </span>
              </label>
              <label className="toggle-row">
                <input
                  type="radio"
                  checked={mode === "samePixel"}
                  disabled={!samePixelReady}
                  onChange={() => setMode("samePixel")}
                />
                <span>
                  <strong>同一像素矩形</strong>
                  <small>
                    {samePixelReady
                      ? "所选图片尺寸一致，按原图像素直通裁剪。"
                      : "所选图片在翻转/旋转后尺寸不一致。"}
                  </small>
                </span>
              </label>
            </div>
            <div className="batch-edit-size-row">
              <Field label="宽">
                <input
                  className="input"
                  type="number"
                  min={1}
                  value={canvasWidth || ""}
                  disabled={mode === "samePixel"}
                  onChange={(event) => {
                    setCanvasTouched(true);
                    setCanvasWidth(Math.max(1, Number(event.target.value) || 0));
                  }}
                />
              </Field>
              <Field label="高">
                <input
                  className="input"
                  type="number"
                  min={1}
                  value={canvasHeight || ""}
                  disabled={mode === "samePixel"}
                  onChange={(event) => {
                    setCanvasTouched(true);
                    setCanvasHeight(Math.max(1, Number(event.target.value) || 0));
                  }}
                />
              </Field>
              <Field label="适配">
                <select
                  className="input"
                  value={fit}
                  disabled={mode === "samePixel"}
                  onChange={(event) => setFit(event.target.value as BatchFit)}
                >
                  <option value="cover">cover（填满，裁掉超出）</option>
                  <option value="contain">contain（完整放入，补底）</option>
                </select>
              </Field>
            </div>
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
              <Button size="sm" variant="ghost" disabled={!crop} onClick={() => setCrop(null)}>
                清除裁剪
              </Button>
              <span className="field-hint">
                输出 {formatDimensions(outputSize.width, outputSize.height)}
              </span>
            </div>
          </section>

          <section className="batch-edit-section">
            <h4>输出</h4>
            <div className="batch-edit-size-row">
              <Field label="格式">
                <select
                  className="input"
                  value={outputFormat}
                  onChange={(event) => {
                    const next = event.target.value as BatchOutputFormat;
                    setOutputFormat(next);
                    if (next === "png") setQuality(null);
                  }}
                >
                  {(Object.keys(FORMAT_LABELS) as BatchOutputFormat[]).map((value) => (
                    <option key={value} value={value}>
                      {FORMAT_LABELS[value]}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label="质量" hint={qualityLabel(outputFormat, quality)}>
                <input
                  className="batch-edit-quality"
                  type="range"
                  min={1}
                  max={100}
                  disabled={outputFormat === "png"}
                  value={
                    quality ??
                    (outputFormat === "keep"
                      ? 90
                      : defaultQualityFor(outputFormat))
                  }
                  onChange={(event) => setQuality(Number(event.target.value))}
                />
              </Field>
            </div>
            <div className="batch-edit-actions">
              <Button size="sm" variant="ghost" disabled={quality == null} onClick={() => setQuality(null)}>
                按格式默认
              </Button>
              {outputFormat === "jpeg" && reference?.hasAlpha ? (
                <span className="field-hint">JPEG 不支持透明，透明区域会合成到白色背景。</span>
              ) : null}
            </div>
          </section>

          <section className="batch-edit-section">
            <h4>参考图</h4>
            <div className="batch-edit-reference">
              {details.map((item) => (
                <button
                  key={item.id}
                  type="button"
                  className={`batch-edit-reference-item${
                    item.id === reference?.id ? " is-active" : ""
                  }`}
                  onClick={() => {
                    setReferenceId(item.id);
                    setCanvasTouched(false);
                  }}
                  title={`使用 #${item.id} 作为参考图`}
                >
                  <img src={thumbnailUrl(item.id, item.thumbVersion)} alt="" draggable={false} />
                </button>
              ))}
            </div>
          </section>
        </div>

        <section className="batch-edit-section">
          <div className="batch-edit-heading">
            <h4>预览</h4>
            <span className="field-hint">按选中顺序均匀取 {sampleIds.length} 张</span>
          </div>
          <div className="batch-edit-previews">
            {details.map((item) => (
              <PreviewTile
                key={item.id}
                detail={item}
                rotate={rotate}
                flipHorizontal={flipHorizontal}
                flipVertical={flipVertical}
                mode={mode}
                fit={fit}
                canvasWidth={canvasWidth}
                canvasHeight={canvasHeight}
                crop={crop}
                reference={item.id === reference?.id}
              />
            ))}
          </div>
        </section>

        <section className="batch-edit-summary">
          <div>
            <strong>确认</strong>
            <span>
              {previewStatus === "loading"
                ? "正在预检…"
                : previewStatus === "error"
                  ? "预检失败"
                  : `${preview?.total ?? selectedIds.length} 张 · 可处理 ${
                      preview?.willProcess ?? "—"
                    } 张`}
            </span>
          </div>
          <dl>
            <div>
              <dt>输出</dt>
              <dd>
                {formatDimensions(outputSize.width, outputSize.height)} ·{" "}
                {FORMAT_LABELS[outputFormat]} · {qualityLabel(outputFormat, quality)}
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
              <dt>会清空</dt>
              <dd>编辑后的图片需要重新生成 embedding 与姿势。</dd>
            </div>
            <div>
              <dt>会丢失</dt>
              <dd>原文件的 EXIF/ICC 不保留；JPEG/WebP 再编码一次会有画质损失。</dd>
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

  const shown = draft ?? crop;
  return (
    <div
      ref={ref}
      className="batch-edit-canvas"
      style={{
        aspectRatio: `${box.width} / ${box.height}`,
        width: `min(100%, ${Math.round(Math.min(680, (360 * box.width) / box.height))}px)`,
      }}
      onPointerDown={(event) => {
        const next = point(event);
        if (!next) return;
        event.currentTarget.setPointerCapture(event.pointerId);
        start.current = next;
        setDraft(null);
      }}
      onPointerMove={(event) => {
        const from = start.current;
        if (!from) return;
        const to = point(event);
        if (!to) return;
        setDraft(rectBetween(from, to, box.width, box.height));
      }}
      onPointerUp={(event) => {
        const from = start.current;
        start.current = null;
        if (!from) return;
        const to = point(event);
        if (!to) return;
        onCrop(rectBetween(from, to, box.width, box.height));
        setDraft(null);
      }}
      onPointerCancel={() => {
        start.current = null;
        setDraft(null);
      }}
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

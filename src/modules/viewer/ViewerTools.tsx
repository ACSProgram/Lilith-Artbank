import { useEffect, useState } from "react";
import {
  Crop,
  FlipHorizontal2,
  FlipVertical2,
  Lock,
  Redo2,
  RotateCcw,
  RotateCw,
  Save,
  Undo2,
  Unlock,
} from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import {
  baseOutputSize,
  clampResize,
  cropToOriginal,
  formatBytes,
  formatDimensions,
  gifStaticPreview,
  normalizeResize,
  outputSize,
  resizeFromHeight,
  resizeFromWidth,
  type GeometryState,
} from "./model";
import type {
  CropRect,
  EditSizeEstimate,
  OutputFormat,
  ResizeSpec,
  ViewerSource,
} from "./types";

interface ViewerToolsProps {
  source: ViewerSource;
  inTemp: boolean;
  geometry: GeometryState;
  cropMode: boolean;
  cropDraft: CropRect | null;
  hasChanges: boolean;
  saving: boolean;
  estimate: EditSizeEstimate | null;
  estimating: boolean;
  canUndo: boolean;
  canRedo: boolean;
  undoLabel: string | null;
  redoLabel: string | null;
  onCommit: (
    updater: (previous: GeometryState) => GeometryState,
    label: string,
    mergeKey?: string,
  ) => void;
  onUndo: () => void;
  onRedo: () => void;
  onRotate: (delta: number) => void;
  onFlip: (axis: "horizontal" | "vertical") => void;
  onEnterCrop: () => void;
  onApplyCrop: () => void;
  onCancelCrop: () => void;
  onClearCrop: () => void;
  onReset: () => void;
  onSave: () => void;
}

const FORMAT_LABELS: Record<OutputFormat, string> = {
  jpeg: "JPEG",
  png: "PNG",
  webp: "WebP",
};

export function ViewerTools({
  source,
  inTemp,
  geometry,
  cropMode,
  cropDraft,
  hasChanges,
  saving,
  estimate,
  estimating,
  canUndo,
  canRedo,
  undoLabel,
  redoLabel,
  onCommit,
  onUndo,
  onRedo,
  onRotate,
  onFlip,
  onEnterCrop,
  onApplyCrop,
  onCancelCrop,
  onClearCrop,
  onReset,
  onSave,
}: ViewerToolsProps) {
  const editable = source.editable && !source.deleted && !saving && !inTemp;
  const base = baseOutputSize(source, geometry);
  const output = outputSize(source, geometry);
  const [widthInput, setWidthInput] = useState(String(output.width));
  const [heightInput, setHeightInput] = useState(String(output.height));
  const [locked, setLocked] = useState(true);

  useEffect(() => {
    setWidthInput(String(output.width));
    setHeightInput(String(output.height));
  }, [output.width, output.height]);

  const applyResize = (resize: ResizeSpec) => {
    onCommit(
      (previous) => ({
        ...previous,
        resize: normalizeResize(clampResize(resize), base),
      }),
      "调整尺寸",
      "resize",
    );
  };

  const changeWidth = (value: string) => {
    setWidthInput(value);
    const parsed = Number(value);
    if (!Number.isFinite(parsed) || parsed < 1) return;
    let next: ResizeSpec;
    if (locked) {
      next = resizeFromWidth(parsed, base, true);
    } else {
      const currentHeight = Number(heightInput);
      next = {
        width: Math.round(parsed),
        height:
          Number.isFinite(currentHeight) && currentHeight >= 1
            ? Math.round(currentHeight)
            : base.height,
      };
    }
    setHeightInput(String(next.height));
    applyResize(next);
  };

  const changeHeight = (value: string) => {
    setHeightInput(value);
    const parsed = Number(value);
    if (!Number.isFinite(parsed) || parsed < 1) return;
    let next: ResizeSpec;
    if (locked) {
      next = resizeFromHeight(parsed, base, true);
    } else {
      const currentWidth = Number(widthInput);
      next = {
        width:
          Number.isFinite(currentWidth) && currentWidth >= 1
            ? Math.round(currentWidth)
            : base.width,
        height: Math.round(parsed),
      };
    }
    setWidthInput(String(next.width));
    applyResize(next);
  };

  const applyPreset = (percent: number) => {
    const next = {
      width: Math.max(1, Math.round((base.width * percent) / 100)),
      height: Math.max(1, Math.round((base.height * percent) / 100)),
    };
    setWidthInput(String(next.width));
    setHeightInput(String(next.height));
    applyResize(next);
  };

  const resetResize = () => {
    setWidthInput(String(base.width));
    setHeightInput(String(base.height));
    onCommit((previous) => ({ ...previous, resize: null }), "还原尺寸");
  };

  const cropSize = geometry.crop
    ? cropToOriginal(geometry.crop, geometry.rotate, source)
    : null;
  const draftSize = cropDraft
    ? cropToOriginal(cropDraft, geometry.rotate, source)
    : null;
  const formatLabel = FORMAT_LABELS[geometry.outputFormat];
  const qualityLabel =
    geometry.outputFormat === "png" ? "无损" : `质量 ${geometry.quality}`;
  const percentChange =
    estimate && source.fileSize > 0
      ? Math.round(((estimate.fileSize - source.fileSize) / source.fileSize) * 100)
      : null;

  return (
    <aside className="viewer-tools" aria-label="编辑工具">
      <div className="viewer-tools-scroll">
        {inTemp ? (
          <p className="viewer-tool-note is-warning">临时文件夹中的图片只读。</p>
        ) : source.deleted ? (
          <p className="viewer-tool-note is-warning">回收站中的图片只读，恢复后才能编辑。</p>
        ) : null}
        {!source.editable && !source.deleted ? (
          <p className="viewer-tool-note is-warning">
            图片超过 {(source.maxEditPixels / 1_000_000).toLocaleString()} 百万像素，仅支持查看。
          </p>
        ) : null}
        {gifStaticPreview(source) ? (
          <p className="viewer-tool-note">
            已生成静态预览，动画不可用；保存仍读取原图。
          </p>
        ) : source.downsampled ? (
          <p className="viewer-tool-note">
            大图按最长边 4096 像素预览；保存仍读取原图。
          </p>
        ) : null}
        {source.format === "gif" ? (
          <p className="viewer-tool-note">
            编辑 GIF 只会保留首帧，保存时请选择 JPEG、PNG 或 WebP。
          </p>
        ) : null}

        <section className="viewer-tool-section">
          <h4>方向</h4>
          <div className="viewer-tool-grid">
            <Button
              size="sm"
              icon={<RotateCcw size={14} />}
              disabled={!editable}
              onClick={() => onRotate(-1)}
            >
              左转 90°
            </Button>
            <Button
              size="sm"
              icon={<RotateCw size={14} />}
              disabled={!editable}
              onClick={() => onRotate(1)}
            >
              右转 90°
            </Button>
            <Button
              size="sm"
              icon={<FlipHorizontal2 size={14} />}
              disabled={!editable}
              onClick={() => onFlip("horizontal")}
            >
              水平翻转
            </Button>
            <Button
              size="sm"
              icon={<FlipVertical2 size={14} />}
              disabled={!editable}
              onClick={() => onFlip("vertical")}
            >
              垂直翻转
            </Button>
          </div>
        </section>

        <section className="viewer-tool-section">
          <div className="viewer-tool-heading">
            <h4>裁剪</h4>
            {cropSize ? (
              <span>
                {formatDimensions(cropSize.width, cropSize.height)}
              </span>
            ) : null}
          </div>
          {cropMode ? (
            <>
              <p className="viewer-tool-hint">
                {draftSize
                  ? `选区 ${formatDimensions(draftSize.width, draftSize.height)}`
                  : "在画布上拖拽画出裁剪区域。"}
              </p>
              <div className="viewer-tool-grid">
                <Button
                  size="sm"
                  variant="primary"
                  disabled={!cropDraft}
                  onClick={onApplyCrop}
                >
                  应用裁剪
                </Button>
                <Button size="sm" onClick={onCancelCrop}>
                  取消
                </Button>
              </div>
            </>
          ) : (
            <div className="viewer-tool-grid">
              <Button
                size="sm"
                icon={<Crop size={14} />}
                disabled={!editable}
                onClick={onEnterCrop}
              >
                {geometry.crop ? "重新裁剪" : "开始裁剪"}
              </Button>
              {geometry.crop ? (
                <Button size="sm" variant="ghost" disabled={!editable} onClick={onClearCrop}>
                  清除裁剪
                </Button>
              ) : null}
            </div>
          )}
        </section>

        <section className="viewer-tool-section">
          <div className="viewer-tool-heading">
            <h4>调整尺寸</h4>
            <span>
              原图 {formatDimensions(base.width, base.height)}
            </span>
          </div>
          <div className="viewer-size-row">
            <label>
              <span>宽</span>
              <input
                className="input"
                inputMode="numeric"
                value={widthInput}
                disabled={!editable}
                onChange={(event) => changeWidth(event.target.value)}
              />
            </label>
            <button
              type="button"
              className="viewer-lock"
              title={locked ? "锁定纵横比" : "解除纵横比锁定"}
              aria-label={locked ? "锁定纵横比" : "解除纵横比锁定"}
              disabled={!editable}
              onClick={() => setLocked((value) => !value)}
            >
              {locked ? <Lock size={13} /> : <Unlock size={13} />}
            </button>
            <label>
              <span>高</span>
              <input
                className="input"
                inputMode="numeric"
                value={heightInput}
                disabled={!editable}
                onChange={(event) => changeHeight(event.target.value)}
              />
            </label>
          </div>
          <div className="viewer-presets">
            {[100, 75, 50, 25].map((percent) => (
              <button
                key={percent}
                type="button"
                disabled={!editable}
                onClick={() => applyPreset(percent)}
              >
                {percent}%
              </button>
            ))}
            <button type="button" disabled={!editable} onClick={resetResize}>
              还原
            </button>
          </div>
          <p className="viewer-tool-hint">
            结果尺寸 {formatDimensions(output.width, output.height)}
          </p>
        </section>

        <section className="viewer-tool-section">
          <h4>输出</h4>
          <div className="viewer-output-row">
            <select
              className="input"
              value={geometry.outputFormat}
              disabled={!editable}
              aria-label="输出格式"
              onChange={(event) => {
                const outputFormat = event.target.value as OutputFormat;
                onCommit(
                  (previous) => ({ ...previous, outputFormat }),
                  `输出 ${FORMAT_LABELS[outputFormat]}`,
                );
              }}
            >
              <option value="jpeg">JPEG</option>
              <option value="png">PNG</option>
              <option value="webp">WebP</option>
            </select>
            <span className="viewer-quality-label">{qualityLabel}</span>
          </div>
          {geometry.outputFormat !== "png" ? (
            <input
              className="viewer-quality"
              type="range"
              min={1}
              max={100}
              value={geometry.quality}
              disabled={!editable}
              aria-label="输出质量"
              onChange={(event) => {
                const quality = Number(event.target.value);
                onCommit(
                  (previous) => ({ ...previous, quality }),
                  `输出 ${formatLabel} q${quality}`,
                  "quality",
                );
              }}
            />
          ) : null}
          {geometry.outputFormat === "jpeg" && source.hasAlpha ? (
            <p className="viewer-tool-hint">JPEG 不支持透明，透明区域会合成到白色背景。</p>
          ) : null}
          {hasChanges ? (
            estimating ? (
              <p className="viewer-tool-hint">正在估算体积…</p>
            ) : estimate && estimate.fileSize > 0 ? (
              <p className="viewer-tool-hint">
                保存后{estimate.exact ? "" : "约"} {formatBytes(estimate.fileSize)}
                {percentChange != null
                  ? `（原 ${formatBytes(source.fileSize)}，${percentChange > 0 ? "+" : ""}${percentChange}%）`
                  : ""}
              </p>
            ) : null
          ) : null}
          {geometry.outputFormat !== "png" && hasChanges ? (
            <p className="viewer-tool-hint">
              保存会重新编码，画质会下降，原始 EXIF 信息不会保留。
            </p>
          ) : null}
        </section>
      </div>

      <div className="viewer-tools-footer">
        <Button
          variant="primary"
          icon={saving ? <Spinner size={13} /> : <Save size={14} />}
          disabled={!editable || !hasChanges || cropMode}
          onClick={onSave}
        >
          {saving ? "保存中…" : "保存修改"}
        </Button>
        <Button
          variant="ghost"
          icon={<RotateCcw size={14} />}
          disabled={!editable || !hasChanges}
          onClick={onReset}
        >
          重置
        </Button>
        <div className="viewer-history-row">
          <button
            type="button"
            disabled={!editable || !canUndo}
            onClick={onUndo}
            title={canUndo && undoLabel ? `撤销：${undoLabel}` : "没有可撤销的操作"}
          >
            <Undo2 size={14} />
            {canUndo ? `撤销：${undoLabel ?? "上一步"}` : "撤销"}
          </button>
          <button
            type="button"
            disabled={!editable || !canRedo}
            onClick={onRedo}
            title={canRedo && redoLabel ? `重做：${redoLabel}` : "没有可重做的操作"}
          >
            <Redo2 size={14} />
            {canRedo ? `重做：${redoLabel ?? "下一步"}` : "重做"}
          </button>
        </div>
        <p>保存会生成新文件并更新缩略图；标签、评分、描述与集合保留，AI 特征会被清除，需要在批量任务里重新生成。</p>
      </div>
    </aside>
  );
}

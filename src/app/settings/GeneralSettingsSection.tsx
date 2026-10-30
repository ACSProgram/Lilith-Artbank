import { Field } from "../../shared/components/Field";
import type { Settings, ThumbFit } from "../types";
import { Panel, ValueField, type SettingsPatch } from "./SettingsPanel";

const FIT_CHOICES: { value: ThumbFit; label: string }[] = [
  { value: "cover", label: "裁剪铺满" },
  { value: "contain", label: "完整显示" },
];

export function GeneralSettingsSection({
  draft,
  patch,
}: {
  draft: Settings;
  patch: SettingsPatch;
}) {
  return (
    <Panel title="浏览">
      <ValueField label="缩略图尺寸" value={`${draft.ui.gridThumbPx} px`}>
        <input
          className="range"
          type="range"
          min={96}
          max={512}
          step={8}
          value={draft.ui.gridThumbPx}
          onChange={(event) =>
            patch((current) => ({
              ...current,
              ui: { ...current.ui, gridThumbPx: Number(event.target.value) },
            }))
          }
        />
      </ValueField>

      <div className="settings-field">
        <span className="field-label">缩略图填充</span>
        <div className="settings-choices">
          {FIT_CHOICES.map((choice) => (
            <button
              key={choice.value}
              type="button"
              className={`settings-choice${draft.ui.thumbFit === choice.value ? " is-active" : ""}`}
              aria-pressed={draft.ui.thumbFit === choice.value}
              onClick={() =>
                patch((current) => ({
                  ...current,
                  ui: { ...current.ui, thumbFit: choice.value },
                }))
              }
            >
              {choice.label}
            </button>
          ))}
        </div>
      </div>

      <ValueField label="缩略图缓存" value={`${draft.ui.thumbCacheCount} 张`}>
        <input
          className="range"
          type="range"
          min={256}
          max={8192}
          step={128}
          value={draft.ui.thumbCacheCount}
          onChange={(event) =>
            patch((current) => ({
              ...current,
              ui: { ...current.ui, thumbCacheCount: Number(event.target.value) },
            }))
          }
        />
      </ValueField>

      <Field
        label="操作记录步数"
        hint="减少后更早的记录会立即清理，永久删除的文件随之释放。"
      >
        <input
          className="input"
          type="number"
          min={10}
          max={500}
          value={draft.history.maxSteps}
          onChange={(event) =>
            patch((current) => ({
              ...current,
              history: { ...current.history, maxSteps: Number(event.target.value) },
            }))
          }
        />
      </Field>
    </Panel>
  );
}

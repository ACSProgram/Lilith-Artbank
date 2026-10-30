import { Button } from "../../shared/components/Button";
import type { Settings } from "../types";
import { Panel, type SettingsPatch } from "./SettingsPanel";

export function MaintenanceSettingsSection({
  draft,
  patch,
  onOpenShortcuts,
}: {
  draft: Settings;
  patch: SettingsPatch;
  onOpenShortcuts: () => void;
}) {
  return (
    <Panel title="维护与帮助">
      <label className="toggle-row">
        <input
          type="checkbox"
          checked={draft.maintenance.autoCompactThumbs}
          onChange={(event) =>
            patch((current) => ({
              ...current,
              maintenance: {
                ...current.maintenance,
                autoCompactThumbs: event.target.checked,
              },
            }))
          }
        />
        <span>
          <strong>空闲时自动回收缩略图空间</strong>
          <small>
            人走开三分钟且没有任务在跑时自动压掉缩略图段文件里的空洞；关闭后仍可手动「立即回收」。
          </small>
        </span>
      </label>
      <div className="settings-field">
        <span className="field-label">快捷键</span>
        <div className="settings-inline-actions">
          <Button size="sm" variant="secondary" onClick={onOpenShortcuts}>
            查看快捷键
          </Button>
          <span className="settings-note">随时按 ? 也能打开。</span>
        </div>
      </div>
    </Panel>
  );
}

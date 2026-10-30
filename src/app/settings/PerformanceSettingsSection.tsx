import { Gauge } from "lucide-react";
import { Field } from "../../shared/components/Field";
import type { Settings } from "../types";
import { Panel, type SettingsPatch } from "./SettingsPanel";

function numberOrNull(value: string): number | null {
  if (!value.trim()) return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

export function PerformanceSettingsSection({
  draft,
  patch,
}: {
  draft: Settings;
  patch: SettingsPatch;
}) {
  return (
    <Panel title="性能">
      <div className="settings-grid">
        <Field label="读取连接数">
          <input
            className="input"
            type="number"
            min={1}
            max={16}
            value={draft.performance.readerConnections}
            onChange={(event) =>
              patch((current) => ({
                ...current,
                performance: {
                  ...current.performance,
                  readerConnections: Number(event.target.value),
                },
              }))
            }
          />
        </Field>
        <Field label="后台图像工作线程">
          <input
            className="input"
            type="number"
            min={1}
            max={128}
            value={draft.performance.workerThreads ?? ""}
            onChange={(event) =>
              patch((current) => ({
                ...current,
                performance: {
                  ...current.performance,
                  workerThreads: numberOrNull(event.target.value),
                },
              }))
            }
            placeholder="自动"
          />
        </Field>
      </div>
      <div className="settings-note">
        <Gauge size={13} />
        图像工作线程改动对之后开始的工作生效；读取连接数在下次打开图库后生效。
      </div>
    </Panel>
  );
}

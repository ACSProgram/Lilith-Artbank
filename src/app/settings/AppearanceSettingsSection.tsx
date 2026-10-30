import { Monitor, Moon, Sun } from "lucide-react";
import type { Settings, ThemeMode } from "../types";
import { Panel, type SettingsPatch } from "./SettingsPanel";

const THEME_CHOICES: { value: ThemeMode; label: string; icon: typeof Sun }[] = [
  { value: "light", label: "浅色", icon: Sun },
  { value: "dark", label: "深色", icon: Moon },
  { value: "system", label: "跟随系统", icon: Monitor },
];

export function AppearanceSettingsSection({
  draft,
  patch,
}: {
  draft: Settings;
  patch: SettingsPatch;
}) {
  return (
    <Panel title="主题">
      <div className="settings-choices">
        {THEME_CHOICES.map(({ value, label, icon: Icon }) => (
          <button
            key={value}
            type="button"
            className={`settings-choice${draft.ui.theme === value ? " is-active" : ""}`}
            aria-pressed={draft.ui.theme === value}
            onClick={() =>
              patch((current) => ({ ...current, ui: { ...current.ui, theme: value } }))
            }
          >
            <Icon size={15} />
            {label}
          </button>
        ))}
      </div>
    </Panel>
  );
}

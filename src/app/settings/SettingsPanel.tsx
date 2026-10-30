import type { ReactNode } from "react";
import type { Settings } from "../types";

/** 草稿更新器；分区共用一个，克隆与脏标记仍由 SettingsDialog 持有。 */
export type SettingsPatch = (update: (current: Settings) => Settings) => void;

export function Panel({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <section className="settings-panel">
      <header className="settings-panel-head">
        <h3>{title}</h3>
      </header>
      <div className="settings-panel-body">{children}</div>
    </section>
  );
}

/** 标签 + 右侧当前值；滑块这类“调多少”的控件用它，省掉另起一行解释单位。 */
export function ValueField({
  label,
  value,
  children,
}: {
  label: string;
  value: string;
  children: ReactNode;
}) {
  return (
    <label className="settings-field">
      <span className="settings-field-head">
        <span className="field-label">{label}</span>
        <span className="settings-value">{value}</span>
      </span>
      {children}
    </label>
  );
}

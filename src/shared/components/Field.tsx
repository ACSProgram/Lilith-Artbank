import type { ReactNode } from "react";

interface FieldProps {
  label: string;
  hint?: string;
  htmlFor?: string;
  /** 字段内有多个可聚焦控件时使用：`<label>` 会把点击转发给第一个控件。 */
  plain?: boolean;
  children: ReactNode;
}

export function Field({ label, hint, htmlFor, plain = false, children }: FieldProps) {
  if (plain) {
    return (
      <div className="field" role="group" aria-label={label}>
        <span className="field-label">{label}</span>
        {children}
        {hint ? <span className="field-hint">{hint}</span> : null}
      </div>
    );
  }
  return (
    <label className="field" htmlFor={htmlFor}>
      <span className="field-label">{label}</span>
      {children}
      {hint ? <span className="field-hint">{hint}</span> : null}
    </label>
  );
}

import type { ReactNode } from "react";

interface FieldProps {
  label: string;
  hint?: string;
  htmlFor?: string;
  children: ReactNode;
}

export function Field({ label, hint, htmlFor, children }: FieldProps) {
  return (
    <label className="field" htmlFor={htmlFor}>
      <span className="field-label">{label}</span>
      {children}
      {hint ? <span className="field-hint">{hint}</span> : null}
    </label>
  );
}

export function Section({
  title,
  description,
  children,
}: {
  title: string;
  description?: string;
  children: ReactNode;
}) {
  return (
    <section className="settings-section">
      <div className="settings-section-heading">
        <h3>{title}</h3>
        {description ? <p>{description}</p> : null}
      </div>
      <div className="settings-section-body">{children}</div>
    </section>
  );
}

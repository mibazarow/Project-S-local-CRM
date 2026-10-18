import type { ReactNode } from "react";

// Presentational wrappers for the redesigned settings layout: a titled card
// block and a label + control + hint field row. No state, no behavior.
export function Card({
  title,
  desc,
  children,
}: {
  title: string;
  desc?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="set-card">
      <div className="set-card-head">
        <div className="set-card-title">{title}</div>
        {desc != null && <div className="set-card-desc">{desc}</div>}
      </div>
      {children}
    </section>
  );
}

export function Field({
  label,
  hint,
  error,
  wide,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  wide?: boolean;
  children?: ReactNode;
}) {
  return (
    <div className={wide ? "set-field wide" : "set-field"}>
      <div className="set-field-info">
        <label className="set-field-label">{label}</label>
        {hint != null && <div className="set-field-hint">{hint}</div>}
        {error && <div className="set-field-hint set-field-error">{error}</div>}
      </div>
      {children != null && <div className="set-field-control">{children}</div>}
    </div>
  );
}

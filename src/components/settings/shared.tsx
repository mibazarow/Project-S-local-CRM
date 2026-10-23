import {
  cloneElement,
  isValidElement,
  useId,
  type ReactElement,
  type ReactNode,
} from "react";
import Switch from "../shared/Switch";

// Presentational wrappers for the redesigned settings layout: a titled card
// block and a label + control + hint field row. No state; Field only links the
// label to its control by id.
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

// Only a single labelable control can be named by one <label for>; anything else
// (a row of buttons, a plain value) keeps the label as plain text.
const LABELABLE = new Set(["input", "select", "textarea"]);

function namedControl(children: ReactNode, id: string): ReactNode {
  if (!isValidElement(children)) return null;
  const child = children as ReactElement<{ id?: string }>;
  const labelable =
    typeof child.type === "string" ? LABELABLE.has(child.type) : child.type === Switch;
  return labelable ? cloneElement(child, { id }) : null;
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
  const id = useId();
  const control = children != null ? namedControl(children, id) : null;
  return (
    <div className={wide ? "set-field wide" : "set-field"}>
      <div className="set-field-info">
        <label className="set-field-label" htmlFor={control ? id : undefined}>
          {label}
        </label>
        {hint != null && <div className="set-field-hint">{hint}</div>}
        {error && <div className="set-field-hint set-field-error">{error}</div>}
      </div>
      {children != null && (
        <div className="set-field-control">{control ?? children}</div>
      )}
    </div>
  );
}

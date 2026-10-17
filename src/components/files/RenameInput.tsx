import { useEffect, useRef } from "react";

interface Props {
  value: string;
  // Files: pre-select the base name without the extension (SPEC).
  selectBase?: boolean;
  placeholder?: string;
  onCommit: (name: string) => void;
  onCancel: () => void;
}

export default function RenameInput({ value, selectBase, placeholder, onCommit, onCancel }: Props) {
  const ref = useRef<HTMLInputElement>(null);
  const done = useRef(false);

  useEffect(() => {
    const input = ref.current;
    if (!input) return;
    input.focus();
    const dot = value.lastIndexOf(".");
    input.setSelectionRange(0, selectBase && dot > 0 ? dot : value.length);
  }, [value, selectBase]);

  const finish = (commit: boolean) => {
    if (done.current) return;
    done.current = true;
    const name = ref.current?.value.trim() ?? "";
    if (commit && name) onCommit(name);
    else onCancel();
  };

  return (
    <input
      ref={ref}
      className="rename-input"
      defaultValue={value}
      placeholder={placeholder}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        if (e.key === "Enter") finish(true);
        if (e.key === "Escape") finish(false);
      }}
      onBlur={() => finish(true)}
    />
  );
}

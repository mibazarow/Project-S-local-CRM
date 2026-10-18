import type { ReactNode } from "react";

export interface SegmentedOption<T extends string> {
  value: T;
  label?: string;
  icon?: ReactNode;
  title?: string;
  disabled?: boolean;
  className?: string;
}

interface Props<T extends string> {
  value: T;
  options: readonly SegmentedOption<T>[];
  onChange: (value: T) => void;
  wrapperClassName?: string;
  buttonClassName?: string;
  title?: string;
}

export default function Segmented<T extends string>({
  value,
  options,
  onChange,
  wrapperClassName = "segmented",
  buttonClassName = "seg",
  title,
}: Props<T>) {
  return (
    <div className={wrapperClassName} title={title}>
      {options.map((option) => (
        <button
          key={option.value}
          className={
            option.value === value
              ? `${buttonClassName} active${option.className ? ` ${option.className}` : ""}`
              : `${buttonClassName}${option.className ? ` ${option.className}` : ""}`
          }
          title={option.title}
          disabled={option.disabled}
          onClick={() => onChange(option.value)}
        >
          {option.icon ?? option.label}
        </button>
      ))}
    </div>
  );
}

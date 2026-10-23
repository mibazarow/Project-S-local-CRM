import type { ComponentPropsWithRef } from "react";

export interface SwitchProps
  extends Omit<ComponentPropsWithRef<"input">, "type" | "className"> {
  // The visible control is the label: tooltips and click handling live there.
  labelProps?: Omit<ComponentPropsWithRef<"label">, "ref" | "className">;
}

export default function Switch({ labelProps, ...input }: SwitchProps) {
  return (
    <label {...labelProps} className="switch">
      <input type="checkbox" {...input} />
      <span className="track" />
    </label>
  );
}

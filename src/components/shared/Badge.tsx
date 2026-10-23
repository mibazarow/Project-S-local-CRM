import type { ComponentPropsWithRef } from "react";

const TONES = {
  active: "st-active",
  error: "st-error",
  disabled: "st-disabled",
} as const;

export type BadgeTone = keyof typeof TONES;

export interface BadgeProps extends ComponentPropsWithRef<"span"> {
  tone?: BadgeTone;
}

export default function Badge({ tone, className, ...rest }: BadgeProps) {
  const cls = ["badge", tone ? TONES[tone] : null, className ?? null]
    .filter(Boolean)
    .join(" ");
  return <span className={cls} {...rest} />;
}

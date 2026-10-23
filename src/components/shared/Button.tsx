import type { ComponentPropsWithRef } from "react";

const VARIANTS = {
  primary: "primary-btn",
  ghost: "ghost-btn",
  icon: "icon-btn",
  mini: "mini-btn",
} as const;

export type ButtonVariant = keyof typeof VARIANTS;

export interface ButtonProps extends ComponentPropsWithRef<"button"> {
  variant?: ButtonVariant;
  size?: "small";
}

export default function Button({ variant, size, className, ...rest }: ButtonProps) {
  const cls = [variant ? VARIANTS[variant] : null, size ?? null, className ?? null]
    .filter(Boolean)
    .join(" ");
  return <button className={cls || undefined} {...rest} />;
}

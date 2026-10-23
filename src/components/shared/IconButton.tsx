import Button, { type ButtonProps } from "./Button";

// Icon-only buttons must carry an accessible name: title or aria-label is required.
export type IconButtonProps = Omit<ButtonProps, "variant" | "title" | "aria-label"> &
  ({ title: string; "aria-label"?: string } | { "aria-label": string; title?: string });

export default function IconButton({ size, className, ...rest }: IconButtonProps) {
  return <Button variant="icon" size={size} className={className} {...rest} />;
}

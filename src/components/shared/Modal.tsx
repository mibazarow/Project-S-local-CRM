import { useEffect, type ReactNode } from "react";

interface Props {
  onClose: () => void;
  closeOnBackdrop?: boolean;
  closeOnEsc?: boolean;
  backdropClassName?: string;
  className: string;
  children: ReactNode;
}

export default function Modal({
  onClose,
  closeOnBackdrop = true,
  closeOnEsc = false,
  backdropClassName = "modal-backdrop",
  className,
  children,
}: Props) {
  useEffect(() => {
    if (!closeOnEsc) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [closeOnEsc, onClose]);

  return (
    <div
      className={backdropClassName}
      onClick={(e) => {
        if (closeOnBackdrop && e.target === e.currentTarget) onClose();
      }}
    >
      <div className={className}>{children}</div>
    </div>
  );
}

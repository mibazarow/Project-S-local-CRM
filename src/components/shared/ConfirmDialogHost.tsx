import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { registerConfirmHost, type ConfirmRequest } from "../../shared/confirm";

// Single app-level outlet for the async confirm channel (shared/confirm.ts):
// renders the pending request as a modal, Esc/backdrop/cancel resolve false.
export default function ConfirmDialogHost() {
  const [req, setReq] = useState<ConfirmRequest | null>(null);
  const confirmRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    registerConfirmHost(setReq);
    return () => registerConfirmHost(null);
  }, []);

  useEffect(() => {
    if (!req) return;
    const prevFocus = document.activeElement;
    confirmRef.current?.focus();
    const settle = (ok: boolean) => {
      req.resolve(ok);
      setReq(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") settle(false);
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      if (prevFocus instanceof HTMLElement) prevFocus.focus();
    };
  }, [req]);

  if (!req) return null;
  const { opts } = req;
  const settle = (ok: boolean) => {
    req.resolve(ok);
    setReq(null);
  };

  return createPortal(
    <div
      className="confirm-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) settle(false);
      }}
    >
      <div className="confirm-dialog" role="alertdialog" aria-modal="true" aria-label={opts.title}>
        <h3>{opts.title}</h3>
        {opts.message && <p className="confirm-message">{opts.message}</p>}
        <div className="confirm-actions">
          <button className="ghost-btn" onClick={() => settle(false)}>
            {opts.cancelLabel ?? "Отмена"}
          </button>
          <button
            ref={confirmRef}
            className={opts.danger ? "primary-btn danger" : "primary-btn"}
            onClick={() => settle(true)}
          >
            {opts.confirmLabel ?? "ОК"}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}

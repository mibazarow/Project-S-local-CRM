import { useEffect, useState } from "react";

// Minimal app-wide toast channel: a module-level emitter plus one mounted
// <Toasts/> outlet. Cross-entity quick actions report success/errors through
// it without threading feedback state through every view.

export type ToastKind = "ok" | "err";

interface ToastItem {
  id: number;
  kind: ToastKind;
  text: string;
}

let nextToastId = 1;
const listeners = new Set<(t: ToastItem) => void>();

export function toast(kind: ToastKind, text: string): void {
  const item: ToastItem = { id: nextToastId++, kind, text };
  listeners.forEach((fn) => fn(item));
}

export default function Toasts() {
  const [items, setItems] = useState<ToastItem[]>([]);
  useEffect(() => {
    const push = (t: ToastItem) => {
      setItems((prev) => [...prev, t]);
      setTimeout(() => setItems((prev) => prev.filter((x) => x.id !== t.id)), 4000);
    };
    listeners.add(push);
    return () => {
      listeners.delete(push);
    };
  }, []);
  if (items.length === 0) return null;
  return (
    <div className="toasts" aria-live="polite">
      {items.map((t) => (
        <div key={t.id} className={`toast ${t.kind}`}>
          {t.text}
        </div>
      ))}
    </div>
  );
}

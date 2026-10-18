import { useCallback, useEffect, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";

// Pointer-based drag-and-drop (HTML5 DnD is unreliable in WKWebView).
// The hook owns the drag lifecycle and the floating ghost; the consumer
// resolves the drop target from pointer coordinates and commits the move.

export interface DropTarget {
  // Which list/column the pointer is over (e.g. "list" or a task status).
  container: string;
  // Insertion index among the non-dragged items of that container.
  index: number;
}

interface Options {
  resolveTarget: (clientX: number, clientY: number, draggingId: number) => DropTarget | null;
  onDrop: (draggingId: number, target: DropTarget) => void;
}

const DRAG_THRESHOLD_PX = 5;
const GHOST_ROTATE_DEG = 10;

export function usePointerDrag({ resolveTarget, onDrop }: Options) {
  const [draggingId, setDraggingId] = useState<number | null>(null);
  const [dropTarget, setDropTarget] = useState<DropTarget | null>(null);

  const resolveRef = useRef(resolveTarget);
  const onDropRef = useRef(onDrop);
  resolveRef.current = resolveTarget;
  onDropRef.current = onDrop;

  // True after a real drag started; the click that follows pointerup must
  // not open/rename the item. Consumers call consumeSuppressedClick() in
  // their click handler and skip the action when it returns true.
  const suppressClickRef = useRef(false);
  const abortRef = useRef<(() => void) | null>(null);

  useEffect(
    () => () => {
      abortRef.current?.();
    },
    [],
  );

  const consumeSuppressedClick = useCallback(() => {
    const suppressed = suppressClickRef.current;
    suppressClickRef.current = false;
    return suppressed;
  }, []);

  const onPointerDown = useCallback((e: ReactPointerEvent<HTMLElement>, id: number) => {
    if (e.button !== 0 || abortRef.current) return;
    const origin = e.target as HTMLElement;
    if (origin.closest("button, input, textarea, select, [contenteditable]")) return;
    suppressClickRef.current = false;

    const el = e.currentTarget;
    const rect = el.getBoundingClientRect();
    const startX = e.clientX;
    const startY = e.clientY;
    const grabX = startX - rect.left;
    const grabY = startY - rect.top;
    let active = false;
    let ghost: HTMLElement | null = null;
    let finished = false;

    const move = (ev: PointerEvent) => {
      if (finished) return;
      if (!active) {
        if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < DRAG_THRESHOLD_PX) return;
        active = true;
        suppressClickRef.current = true;
        document.body.classList.add("dnd-active");
        ghost = el.cloneNode(true) as HTMLElement;
        ghost.classList.add("drag-ghost");
        ghost.removeAttribute("data-id");
        ghost.style.position = "fixed";
        ghost.style.left = "0";
        ghost.style.top = "0";
        ghost.style.width = `${rect.width}px`;
        ghost.style.height = `${rect.height}px`;
        ghost.style.margin = "0";
        ghost.style.zIndex = "1000";
        ghost.style.pointerEvents = "none";
        document.body.appendChild(ghost);
        setDraggingId(id);
      }
      ev.preventDefault();
      if (ghost) {
        ghost.style.transform = `translate3d(${ev.clientX - grabX}px, ${ev.clientY - grabY}px, 0) rotate(${GHOST_ROTATE_DEG}deg)`;
      }
      const next = resolveRef.current(ev.clientX, ev.clientY, id);
      setDropTarget((prev) =>
        prev?.container === next?.container && prev?.index === next?.index ? prev : next,
      );
    };

    const finish = (commit: boolean, ev: PointerEvent | null) => {
      if (finished) return;
      finished = true;
      detach();
      ghost?.remove();
      document.body.classList.remove("dnd-active");
      setDraggingId(null);
      setDropTarget(null);
      if (commit && active && ev) {
        const target = resolveRef.current(ev.clientX, ev.clientY, id);
        if (target) onDropRef.current(id, target);
      }
    };

    const up = (ev: PointerEvent) => finish(true, ev);
    const cancel = () => finish(false, null);
    const key = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") finish(false, null);
    };
    const abort = () => finish(false, null);

    const detach = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", cancel);
      window.removeEventListener("keydown", key);
      if (abortRef.current === abort) abortRef.current = null;
    };

    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", cancel);
    window.addEventListener("keydown", key);
    abortRef.current = abort;
  }, []);

  return { draggingId, dropTarget, onPointerDown, consumeSuppressedClick };
}

import { useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import type { UnlistenFn } from "@tauri-apps/api/event";

// Finder drops never reach the HTML5 drop API with usable paths in a Tauri
// webview, so source paths come from the webview drag-drop event channel.
// The hovered destination folder is resolved from the element under the
// pointer (physical px -> CSS px); drop zones are marked with data-drop-dir.
// The channel is global, so drops over a chat drop zone (data-chat-drop,
// handled by useChatAttachments) are ignored here instead of falling back to
// the tree's current folder.
export function useOsFileDrag(
  fallbackDest: () => string,
  onImport: (destRel: string, paths: string[]) => void,
): string | null {
  const [hoverDest, setHoverDest] = useState<string | null>(null);
  const hoverRef = useRef<string | null>(null);
  const fallbackRef = useRef(fallbackDest);
  const importRef = useRef(onImport);
  fallbackRef.current = fallbackDest;
  importRef.current = onImport;

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let cancelled = false;
    const setHover = (value: string | null) => {
      hoverRef.current = value;
      setHoverDest(value);
    };
    const elementUnder = (x: number, y: number) =>
      document.elementFromPoint(x / window.devicePixelRatio, y / window.devicePixelRatio);
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        const payload = event.payload;
        if (payload.type === "enter" || payload.type === "over") {
          const el = elementUnder(payload.position.x, payload.position.y);
          if (el?.closest("[data-chat-drop]")) {
            setHover(null);
            return;
          }
          const zone = el?.closest<HTMLElement>("[data-drop-dir]");
          setHover(zone ? (zone.dataset.dropDir ?? "") : null);
        } else if (payload.type === "drop") {
          const el = elementUnder(payload.position.x, payload.position.y);
          const dest = hoverRef.current ?? fallbackRef.current();
          setHover(null);
          if (el?.closest("[data-chat-drop]")) return;
          if (payload.paths.length > 0) importRef.current(dest, payload.paths);
        } else {
          setHover(null);
        }
      })
      .then((u) => (cancelled ? u() : (unlisten = u)))
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  return hoverDest;
}

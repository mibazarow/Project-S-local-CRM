import { useEffect, useRef, useState, type RefObject } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import type { UnlistenFn } from "@tauri-apps/api/event";
import * as ipc from "../../../api/ipc";

// Pending attachment for the chat input (SPEC 6.1): the absolute source path
// picked via the скрепка dialog or dropped from Finder. The file is copied
// into the inbox only on send (attach_files), so a cancelled message leaves
// no trace. Image chips get a thumbnail preview read straight from the
// source path through the attachment data-url command.
export interface PendingAttachment {
  key: number;
  path: string;
  name: string;
  isImage: boolean;
  previewUrl: string | null;
}

const IMAGE_EXT_RE = /\.(png|jpe?g|gif|webp|svg|bmp|ico|tiff?|heic|heif|avif)$/i;

let nextPendingKey = 1;

// Attach widget state for one chat. The webview drag-drop channel is global
// (same as in useOsFileDrag), so drops are scoped to this chat: a drop only
// counts when the pointer is over the chat's own root element, which the
// caller marks with data-chat-drop. `enabled` gates the whole channel: a chat
// with nothing to attach to (no project selected, notes chat not yet created)
// ignores drops and stays unhighlighted.
export function useChatAttachments(
  rootRef: RefObject<HTMLElement | null>,
  enabled = true,
) {
  const [pending, setPending] = useState<PendingAttachment[]>([]);
  const [dragOver, setDragOver] = useState(false);
  const enabledRef = useRef(enabled);
  enabledRef.current = enabled;

  const addPaths = (paths: string[]) => {
    const items = paths
      .filter((p) => p.startsWith("/"))
      .map((p) => ({
        key: nextPendingKey++,
        path: p,
        name: p.slice(p.lastIndexOf("/") + 1),
        isImage: IMAGE_EXT_RE.test(p),
        previewUrl: null,
      }));
    if (items.length === 0) return;
    setPending((cur) => [...cur, ...items]);
    for (const item of items) {
      if (!item.isImage) continue;
      void ipc
        .attachmentDataUrl(item.path)
        .then((url) =>
          setPending((cur) => cur.map((c) => (c.key === item.key ? { ...c, previewUrl: url } : c))),
        )
        .catch(() => {});
    }
  };
  const addRef = useRef(addPaths);
  addRef.current = addPaths;

  const remove = (key: number) => setPending((cur) => cur.filter((c) => c.key !== key));
  const clear = () => setPending([]);

  const pick = async () => {
    const picked = await open({ multiple: true, directory: false });
    if (!picked) return;
    addPaths(Array.isArray(picked) ? picked : [picked]);
  };

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let cancelled = false;
    const insideChat = (x: number, y: number) => {
      const el = document.elementFromPoint(
        x / window.devicePixelRatio,
        y / window.devicePixelRatio,
      );
      return el != null && (rootRef.current?.contains(el) ?? false);
    };
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        const payload = event.payload;
        if (!enabledRef.current) {
          setDragOver(false);
          return;
        }
        if (payload.type === "enter" || payload.type === "over") {
          setDragOver(insideChat(payload.position.x, payload.position.y));
        } else if (payload.type === "drop") {
          const inside = insideChat(payload.position.x, payload.position.y);
          setDragOver(false);
          if (inside && payload.paths.length > 0) addRef.current(payload.paths);
        } else {
          setDragOver(false);
        }
      })
      .then((u) => (cancelled ? u() : (unlisten = u)))
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  return { pending, dragOver, remove, clear, pick };
}

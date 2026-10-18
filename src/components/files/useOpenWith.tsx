import { useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry, OpenWithApp } from "../../api/types";
import { toMessage } from "./fileOps";
import OpenWithDialog from "./OpenWithDialog";

// Document files (docx/xlsx/pptx/pdf/…): opened externally through the
// remembered app or the chooser; md/txt/images keep the in-app preview.
export function isDocument(entry: FsEntry): boolean {
  return entry.kind === "file" && entry.ext !== "" && !entry.isText && !entry.isImage;
}

// Shared "open externally" flow for the file views and the preview:
// remembered app if set → chooser → system default when no non-stock app
// exists (fs_open_external stays the fallback path).
export function useOpenWith(projectId: number) {
  const [chooser, setChooser] = useState<{ entry: FsEntry; apps: OpenWithApp[] } | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setChooser(null);
    setError(null);
  }, [projectId]);

  const openDocument = async (entry: FsEntry) => {
    setError(null);
    try {
      const remembered = await ipc.getOpenWith(entry.ext);
      if (remembered) {
        await ipc.openWith("project", entry.relPath, projectId, remembered.path);
        return;
      }
      const apps = await ipc.listOpenWithApps(entry.ext);
      if (apps.length === 0) {
        await ipc.fsOpenExternal(projectId, entry.relPath);
        return;
      }
      setChooser({ entry, apps });
    } catch (e) {
      setError(toMessage(e));
    }
  };

  // "Открыть с помощью…": always show the chooser to change the app.
  const showChooser = async (entry: FsEntry) => {
    setError(null);
    try {
      const apps = await ipc.listOpenWithApps(entry.ext);
      setChooser({ entry, apps });
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const pick = async (app: OpenWithApp | null, remember: boolean) => {
    if (!chooser) return;
    const { entry } = chooser;
    if (app) {
      await ipc.openWith("project", entry.relPath, projectId, app.path);
      if (remember) await ipc.rememberOpenWith(entry.ext, app.path);
    } else {
      await ipc.fsOpenExternal(projectId, entry.relPath);
    }
    setChooser(null);
  };

  const dialog = chooser ? (
    <OpenWithDialog
      entry={chooser.entry}
      apps={chooser.apps}
      onPick={pick}
      onClose={() => setChooser(null)}
    />
  ) : null;

  return { openDocument, showChooser, error, dialog };
}

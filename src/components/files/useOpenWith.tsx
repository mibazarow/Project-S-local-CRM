import { useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry, OpenWithApp } from "../../api/types";
import { toMessage } from "./fileOps";
import OpenWithDialog from "./OpenWithDialog";

// Document files (docx/xlsx/pptx/pdf/…): opened externally through the
// chooser; md/txt/images keep the in-app preview.
export function isDocument(entry: FsEntry): boolean {
  return entry.kind === "file" && entry.ext !== "" && !entry.isText && !entry.isImage;
}

// Shared "open in editor" flow for the file views and the preview:
// «Открыть в редакторе» always shows the chooser of compatible apps; the
// remembered per-extension app is marked in it, and picking with
// "remember" stores it again (system default stays the fallback when no
// compatible app exists).
export function useOpenWith(projectId: number) {
  const [chooser, setChooser] = useState<{
    entry: FsEntry;
    apps: OpenWithApp[];
    rememberedPath: string | null;
  } | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setChooser(null);
    setError(null);
  }, [projectId]);

  const showChooser = async (entry: FsEntry) => {
    setError(null);
    try {
      const [apps, remembered] = await Promise.all([
        ipc.listOpenWithApps(entry.ext),
        ipc.getOpenWith(entry.ext),
      ]);
      setChooser({ entry, apps, rememberedPath: remembered?.path ?? null });
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
      rememberedPath={chooser.rememberedPath}
      onPick={pick}
      onClose={() => setChooser(null)}
    />
  ) : null;

  return { showChooser, error, dialog };
}

import { useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import * as ipc from "../../../api/ipc";
import type { ProjectDto } from "../../../api/types";
import { toMessage as errMsg } from "../../../shared/errors";
import { exportStamp } from "../forms";

// Import/export: folder/.zip/Obsidian imports into a project or
// the Notes vault, and project/vault → .zip exports.
export interface ImportExport {
  importTarget: "project" | "notes";
  importProjectId: number | null;
  setImportTarget: (target: "project" | "notes") => void;
  setImportProjectId: (id: number | null) => void;
  importBusy: boolean;
  importNote: string | null;
  importError: string | null;
  runImport: (kind: "folder" | "archive" | "obsidian") => Promise<void>;
  exportProjectId: number | null;
  setExportProjectId: (id: number | null) => void;
  exportBusy: boolean;
  exportNote: string | null;
  exportError: string | null;
  doExport: (kind: "project" | "notes") => Promise<void>;
}

export function useImportExport(projects: ProjectDto[]): ImportExport {
  const [importTarget, setImportTarget] = useState<"project" | "notes">("project");
  const [importProjectId, setImportProjectId] = useState<number | null>(null);
  const [importBusy, setImportBusy] = useState(false);
  const [importNote, setImportNote] = useState<string | null>(null);
  const [importError, setImportError] = useState<string | null>(null);
  const [exportProjectId, setExportProjectId] = useState<number | null>(null);
  const [exportBusy, setExportBusy] = useState(false);
  const [exportNote, setExportNote] = useState<string | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);

  // Import a folder / .zip / Obsidian vault. folder and archive go into the
  // selected target (project needs a chosen project); Obsidian always lands in
  // the Notes vault. The picker is a directory chooser except for the archive.
  const runImport = async (kind: "folder" | "archive" | "obsidian") => {
    setImportBusy(true);
    setImportNote(null);
    setImportError(null);
    try {
      const picked = await open(
        kind === "archive"
          ? {
              multiple: false,
              title: "Импорт архива .zip",
              filters: [{ name: "Zip-архив", extensions: ["zip"] }],
            }
          : {
              directory: true,
              title: kind === "obsidian" ? "Импорт Obsidian vault" : "Импорт папки",
            },
      );
      if (typeof picked !== "string" || !picked) return;
      if (kind !== "obsidian" && importTarget === "project" && importProjectId == null) {
        setImportError("Выбери проект для импорта");
        return;
      }
      const s =
        kind === "obsidian"
          ? await ipc.importObsidian(picked)
          : kind === "folder"
            ? await ipc.importFolder(importTarget, importProjectId, picked)
            : await ipc.importArchive(importTarget, importProjectId, picked);
      const skipped = s.skipped > 0 ? ` · пропущено папок: ${s.skipped}` : "";
      setImportNote(`Импортировано файлов: ${s.importedFiles}${skipped} → ${s.dest}`);
    } catch (e) {
      setImportError(errMsg(e));
    } finally {
      setImportBusy(false);
    }
  };

  // Export a project or the whole Notes vault to a .zip. The save dialog
  // suggests "<name>-<timestamp>.zip"; the user picks the destination folder.
  const doExport = async (kind: "project" | "notes") => {
    setExportBusy(true);
    setExportNote(null);
    setExportError(null);
    try {
      if (kind === "project" && exportProjectId == null) {
        setExportError("Выбери проект для экспорта");
        return;
      }
      const proj = kind === "project" ? projects.find((p) => p.id === exportProjectId) : null;
      const suggested = `${kind === "project" ? (proj?.name ?? "project") : "notes"}-${exportStamp()}.zip`;
      const dest = await save({
        title: kind === "project" ? "Экспорт проекта в .zip" : "Экспорт заметок в .zip",
        defaultPath: suggested,
        filters: [{ name: "Zip-архив", extensions: ["zip"] }],
      });
      if (typeof dest !== "string" || !dest) return;
      const r =
        kind === "project"
          ? await ipc.exportProject(exportProjectId as number, dest)
          : await ipc.exportNotes(dest);
      setExportNote(`Готово: ${r.path} · файлов ${r.files}`);
    } catch (e) {
      setExportError(errMsg(e));
    } finally {
      setExportBusy(false);
    }
  };

  return {
    importTarget,
    importProjectId,
    setImportTarget,
    setImportProjectId,
    importBusy,
    importNote,
    importError,
    runImport,
    exportProjectId,
    setExportProjectId,
    exportBusy,
    exportNote,
    exportError,
    doExport,
  };
}

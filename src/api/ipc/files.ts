// Project file browser IPC (fs_*), the "open with" chooser and the
// trash/version-history reliability commands.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  FsChanged,
  FsEntry,
  OpenWithApp,
  OpenWithResult,
  TrashChanged,
  TrashDto,
  VersionDto,
  VersionRestoreDto,
  VersionsChanged,
} from "../types";

export const fsListDir = (projectId: number, relPath: string) =>
  invoke<FsEntry[]>("fs_list_dir", { projectId, relPath });

export const fsReadText = (projectId: number, relPath: string) =>
  invoke<string>("fs_read_text", { projectId, relPath });

export const fsWriteText = (projectId: number, relPath: string, content: string) =>
  invoke<FsEntry>("fs_write_text", { projectId, relPath, content });

export const fsCreateFolder = (projectId: number, parentRel: string, name: string) =>
  invoke<FsEntry>("fs_create_folder", { projectId, parentRel, name });

export const fsCreateFile = (projectId: number, parentRel: string, name: string) =>
  invoke<FsEntry>("fs_create_file", { projectId, parentRel, name });

export const fsRename = (projectId: number, relPath: string, newName: string) =>
  invoke<FsEntry>("fs_rename", { projectId, relPath, newName });

// File context menu (fs_copy_as merged the former fs_duplicate «Дублировать»).
export const fsCopyAs = (projectId: number, relPath: string) =>
  invoke<FsEntry>("fs_copy_as", { projectId, relPath });

export const fsAbsolutePath = (projectId: number, relPath: string) =>
  invoke<string>("fs_absolute_path", { projectId, relPath });

export const fsRevealInFinder = (projectId: number, relPath: string) =>
  invoke<void>("fs_reveal_in_finder", { projectId, relPath });

export const fsExtractArchive = (projectId: number, relPath: string) =>
  invoke<FsEntry>("fs_extract_archive", { projectId, relPath });

export const fsDelete = (projectId: number, relPath: string) =>
  invoke<void>("fs_delete", { projectId, relPath });

export const fsMove = (projectId: number, fromRel: string, toDirRel: string) =>
  invoke<FsEntry>("fs_move", { projectId, fromRel, toDirRel });

export const fsImport = (projectId: number, destRel: string, sourcePaths: string[]) =>
  invoke<FsEntry[]>("fs_import", { projectId, destRel, sourcePaths });

export const fsOpenExternal = (projectId: number, relPath: string) =>
  invoke<void>("fs_open_external", { projectId, relPath });

// "Open with" chooser: non-stock apps for an extension, the remembered
// per-extension choice, and opening a file with a chosen app.
export const listOpenWithApps = (ext: string) =>
  invoke<OpenWithApp[]>("list_open_with_apps", { ext });

export const getOpenWith = (ext: string) => invoke<OpenWithApp | null>("get_open_with", { ext });

export const rememberOpenWith = (ext: string, appPath: string) =>
  invoke<void>("remember_open_with", { ext, appPath });

export const openWith = (
  scope: "project" | "notes" | "harness",
  path: string,
  projectId?: number,
  appPath?: string,
) =>
  invoke<OpenWithResult>("open_with", {
    scope,
    projectId: projectId ?? null,
    path,
    appPath: appPath ?? null,
  });

export const fsReadBinaryDataUrl = (projectId: number, relPath: string, maxBytes: number) =>
  invoke<string>("fs_read_binary_data_url", { projectId, relPath, maxBytes });

// Trash and version history (SPEC trash/versions slice). version_list/restore
// are keyed by (scope, absolute path); restore returns the stored content and
// the caller writes it back through the scope's own write command.
export const trashList = () => invoke<TrashDto[]>("trash_list");

export const trashRestore = (id: number) => invoke<TrashDto>("trash_restore", { id });

export const trashPurge = (id: number) => invoke<void>("trash_purge", { id });

export const versionList = (scope: string, path: string) =>
  invoke<VersionDto[]>("version_list", { scope, path });

export const versionRestore = (id: number) => invoke<VersionRestoreDto>("version_restore", { id });

export const onFsChanged = (handler: (payload: FsChanged) => void) =>
  listen<FsChanged>("fs-changed", (e) => handler(e.payload));

// Emitted after any trash mutation (delete/restore/purge); refresh trash lists.
export const onTrashChanged = (handler: (payload: TrashChanged) => void) =>
  listen<TrashChanged>("trash-changed", (e) => handler(e.payload));

// Emitted after a version snapshot is taken for (scope, absolute path); the
// version-history panel reloads when its own key matches.
export const onVersionsChanged = (handler: (payload: VersionsChanged) => void) =>
  listen<VersionsChanged>("versions-changed", (e) => handler(e.payload));

// System/onboarding IPC: working dir, license, templates, quick capture,
// style profile and the reindex/embed/search commands.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  EmbedModelProgress,
  EmbedStatus,
  ExportSummary,
  GlobalHit,
  ImportSummary,
  IndexChanged,
  LicenseStatus,
  OnboardingStatus,
  ProjectIndexStats,
  QuickCaptureResult,
  ReindexDone,
  ReindexProgress,
  ReindexStatus,
  TrayAction,
  WorkingDirInfo,
} from "../types";

export const onboardingStatus = () => invoke<OnboardingStatus>("onboarding_status");

export const licenseStatus = () => invoke<LicenseStatus>("license_status");

export const licenseAccept = () => invoke<LicenseStatus>("license_accept");

export const licenseDecline = () => invoke<void>("license_decline");

export const defaultWorkingDir = () => invoke<string>("default_working_dir");

export const chooseWorkingDir = (path: string) =>
  invoke<WorkingDirInfo>("choose_working_dir", { path });

export const getWorkingDir = () => invoke<string | null>("get_working_dir");

export const configDirPath = () => invoke<string>("config_dir_path");

// Import/export (SPEC import/export slice). target selects the landing spot:
// "project" (needs projectId) or "notes". destRel is an optional existing
// subfolder inside the target; "" / null means the target root. Exports take
// an optional explicit destination .zip; null uses the backend default under
// Settings/backups/exports/.
export const importFolder = (
  target: "project" | "notes",
  projectId: number | null,
  sourcePath: string,
  destRel?: string,
) =>
  invoke<ImportSummary>("import_folder", {
    target,
    projectId,
    sourcePath,
    destRel: destRel ?? null,
  });

export const importArchive = (
  target: "project" | "notes",
  projectId: number | null,
  sourcePath: string,
  destRel?: string,
) =>
  invoke<ImportSummary>("import_archive", {
    target,
    projectId,
    sourcePath,
    destRel: destRel ?? null,
  });

export const importObsidian = (sourcePath: string, destRel?: string) =>
  invoke<ImportSummary>("import_obsidian", { sourcePath, destRel: destRel ?? null });

export const exportProject = (projectId: number, destPath?: string) =>
  invoke<ExportSummary>("export_project", { projectId, destPath: destPath ?? null });

export const exportNotes = (destPath?: string) =>
  invoke<ExportSummary>("export_notes", { destPath: destPath ?? null });

// Quick capture (SPEC quick capture slice): one entry from the capture modal
// (global shortcut / tray menu). target "note" appends to the daily
// Notes/Входящие note; target "task" creates a task, projectId null falls
// back to the first project.
export const quickCapture = (
  text: string,
  target: "note" | "task",
  projectId?: number | null,
) => invoke<QuickCaptureResult>("quick_capture", { text, target, projectId: projectId ?? null });

export const reindexAll = () => invoke<ProjectIndexStats[]>("reindex_all");

// Hard rebuild: clears the whole file index (all projects + notes) in one
// transaction and reindexes everything from scratch.
export const reindexHard = () => invoke<ProjectIndexStats[]>("reindex_hard");

// Style profile (SPEC 6.6): reset the global style-of-communication profile the
// agents mirror from the user's messages.
export const styleReset = () => invoke<void>("style_reset");

// Search reindex («Переиндексация поиска»): manual start (no-op while a pass
// runs) and read-only status; the same pass also fires automatically every
// 5 minutes. Progress arrives as "reindex-progress" / "reindex-done".
export const reindexStart = () => invoke<ReindexStatus>("reindex_start");

export const reindexStatus = () => invoke<ReindexStatus>("reindex_status");

export const embedStatus = () => invoke<EmbedStatus>("embed_status");

// One-time download of the EmbeddingGemma 2 model (~300 MB) from Hugging
// Face into Settings/models/embeddinggemma-2/. Returns the new embed status;
// per-file byte progress arrives as "embed-model-progress".
export const downloadEmbedModel = () => invoke<EmbedStatus>("download_embed_model");

// Global cross-entity search (⌘K palette): projects, files, tasks, notes and
// harness chats in one merged, ranked list.
export const globalSearch = (query: string, limit?: number) =>
  invoke<GlobalHit[]>("global_search", { query, limit: limit ?? null });

export const indexChatMessages = () => invoke<number>("index_chat_messages");

export const onIndexChanged = (handler: (payload: IndexChanged) => void) =>
  listen<IndexChanged>("index-changed", (e) => handler(e.payload));

// Emitted by the reindex pass after each unit (project/notes/tasks/chats/memory).
export const onReindexProgress = (handler: (payload: ReindexProgress) => void) =>
  listen<ReindexProgress>("reindex-progress", (e) => handler(e.payload));

// Emitted once when a reindex pass finishes.
export const onReindexDone = (handler: (payload: ReindexDone) => void) =>
  listen<ReindexDone>("reindex-done", (e) => handler(e.payload));

// Emitted by download_embed_model after each chunk of a model file.
export const onEmbedModelProgress = (handler: (payload: EmbedModelProgress) => void) =>
  listen<EmbedModelProgress>("embed-model-progress", (e) => handler(e.payload));

// Emitted by the tray menu after showing the window: routes «Быстрая
// заметка» / «Новая задача» to the capture modal.
export const onTrayAction = (handler: (payload: TrayAction) => void) =>
  listen<TrayAction>("tray-action", (e) => handler(e.payload));

// Emitted by the global CmdOrCtrl+Shift+Space shortcut after showing the
// window: open the capture modal.
export const onQuickCapture = (handler: () => void) =>
  listen<unknown>("quick-capture", () => handler());

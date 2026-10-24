// Notes vault IPC (top-level Notes/ folder) and the notes-chat commands.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { FsEntry, MessageAttachmentDto, MessageDto, NotesChanged } from "../types";

// Notes vault (top-level Notes/ folder): markdown notes with folders, all
// paths vault-relative. Mutations emit "notes-changed"; agent tool writes
// arrive later as "index-changed" with projectId = -1 (NOTES_SCOPE_ID).
export const notesEnsure = () => invoke<string>("notes_ensure");

export const notesTree = () => invoke<FsEntry[]>("notes_tree");

export const notesList = (rel?: string) => invoke<FsEntry[]>("notes_list", { rel: rel ?? null });

export const notesRead = (rel: string) => invoke<string>("notes_read", { rel });

// Binary preview read for non-md vault files (images/PDF as base64 data
// URLs); text files keep notesRead. Same caps as fsReadBinaryDataUrl.
export const notesReadBinaryDataUrl = (rel: string, maxBytes: number) =>
  invoke<string>("notes_read_binary_data_url", { rel, maxBytes });

export const notesWrite = (rel: string, content: string) =>
  invoke<FsEntry>("notes_write", { rel, content });

export const notesCreate = (rel: string, content?: string) =>
  invoke<FsEntry>("notes_create", { rel, content: content ?? null });

export const notesRename = (rel: string, newName: string) =>
  invoke<FsEntry>("notes_rename", { rel, newName });

export const notesMove = (fromRel: string, toDirRel: string) =>
  invoke<FsEntry>("notes_move", { fromRel, toDirRel });

// Finder drag-and-drop copy into the vault (SPEC 4.2): sources are copied,
// never moved; destRel "" is the vault root. Emits "notes-changed" per item.
export const notesImport = (destRel: string, sourcePaths: string[]) =>
  invoke<FsEntry[]>("notes_import", { destRel, sourcePaths });

export const notesDelete = (rel: string) => invoke<void>("notes_delete", { rel });

// Reveals a vault entry in the OS file manager (Finder/Explorer); rel "" is
// the Notes folder itself. Escapes and missing entries are rejected backend-side.
export const notesReveal = (rel: string) => invoke<void>("notes_reveal", { rel });

// Notes chat: one continuous chat per working dir; events route by
// "notes: true" on agent-event / agent-confirmation.
export const ensureNotesChat = () => invoke<number>("ensure_notes_chat");

export const notesChatHistory = () => invoke<MessageDto[]>("notes_chat_history");

export const sendNotesMessage = (text: string, attachments?: MessageAttachmentDto[]) =>
  invoke<number>("send_notes_message", { text, attachments: attachments ?? null });

export const cancelNotesRun = () => invoke<boolean>("cancel_notes_run");

// Emitted by the notes_* mutation commands after a note or folder changed.
export const onNotesChanged = (handler: (payload: NotesChanged) => void) =>
  listen<NotesChanged>("notes-changed", (e) => handler(e.payload));

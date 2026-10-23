// Project file browser DTOs, "open with" chooser, search hits and the
// trash/version-history reliability rows.

export interface FsEntry {
  name: string;
  relPath: string;
  kind: "dir" | "file";
  ext: string;
  size: number;
  modified: number;
  isMarkdown: boolean;
  isText: boolean;
  isImage: boolean;
}

// "Open with" chooser: a non-stock macOS app offered for a file extension.
export interface OpenWithApp {
  name: string;
  path: string;
  recommended: boolean;
}

export interface OpenWithResult {
  opened: boolean;
  app?: string | null;
}

export interface SearchHit {  relPath: string;
  name: string;
  kind: string;
  score: number;
  snippet: string;
  matchTypes: string[];
  modified: number;
  taskId?: number | null;
  status?: string | null;
  overdue?: boolean | null;
}

export interface FsChanged {
  projectId: number;
  relPath: string;
}

// reliability::TrashDto — one row of trash_list. kind is "file" | "dir" |
// "task"; scope is "project" | "notes" | "harness". storedPath is empty for
// task snapshots (the whole item lives in `meta`).
export interface TrashDto {
  id: number;
  scope: string;
  kind: string;
  originalPath: string;
  storedPath: string;
  name: string;
  deletedAt: string;
  meta: string | null;
  // Filled only when a restored task is returned, so callers can refresh the
  // owning project's board.
  projectId?: number | null;
}

// reliability::VersionDto — one snapshot row of version_list (keyed by the
// absolute path of the file).
export interface VersionDto {
  id: number;
  scope: string;
  path: string;
  bytes: number;
  createdAt: string;
}

// reliability::VersionRestoreDto — result of version_restore; the caller
// writes `content` back through the scope's own write command.
export interface VersionRestoreDto {
  id: number;
  scope: string;
  path: string;
  content: string;
}

// lib.rs TrashChanged — payload of the "trash-changed" event.
export interface TrashChanged {
  id: number | null;
}

// lib.rs VersionsChanged — payload of the "versions-changed" event, keyed by
// the same (scope, absolute path) pair as version_list.
export interface VersionsChanged {
  scope: string;
  path: string;
}

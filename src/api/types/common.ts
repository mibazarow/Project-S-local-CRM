// Cross-cutting DTOs: onboarding/license, global search, errors, tray, memory.

export interface OnboardingStatus {
  configured: boolean;
  workingDir: string | null;
}

export interface LicenseStatus {
  required: boolean;
  accepted: boolean;
  version: number;
}

export interface WorkingDirInfo {
  workingDir: string;
  configDir: string;
}

// search::GlobalHit — one hit of the cross-entity ⌘K palette search.
// kind selects the navigation target; the id fields tag it (project/task/
// session id, files_fts rowid for file/note hits). Snippets carry FTS5
// markers ("<<match>>").
export interface GlobalHit {
  kind: "project" | "file" | "task" | "note" | "chat";
  id: number;
  title: string;
  subtitle: string;
  snippet: string;
  score: number;
  projectId?: number | null;
  sessionId?: number | null;
  taskId?: number | null;
  relPath?: string | null;
}

// AppError is serialized by the backend as { code, message }.
export interface AppErrorPayload {
  code: string;
  message: string;
}

// lib.rs TrayAction — payload of the "tray-action" event emitted by the
// menu-bar menu after the window was shown; "quit" is handled natively.
export interface TrayAction {
  action: "open" | "quick-note" | "new-task";
}

// memory::MemoryDto — one global memory record (SPEC 6.3).
export interface MemoryDto {
  id: number;
  content: string;
  projectId: number | null;
  tags: string[];
  source: string | null;
  createdAt: string;
  updatedAt: string;
}

// memory::MemoryChanged — payload of the "memory-changed" event.
export interface MemoryChanged {
  id: number | null;
}

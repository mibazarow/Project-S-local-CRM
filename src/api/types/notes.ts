// Notes vault DTOs and the quick-capture result.

// lib.rs NotesChanged — payload of the "notes-changed" event emitted by the
// notes_* mutation commands.
export interface NotesChanged {
  relPath: string;
}

// lib.rs QuickCaptureResult — result of the quick_capture command.
export interface QuickCaptureResult {
  kind: "note" | "task";
  path: string | null;
  taskId: number | null;
}

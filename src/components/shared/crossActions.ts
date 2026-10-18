import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import { useAppActions } from "../../state/AppStore";

// Shared helpers for the cross-entity quick actions that connect files,
// tasks, chat messages and the project agent chat.

// First non-empty line of a text, sanitized into a Notes-vault file name.
export function noteNameFromText(raw: string): string {
  const line =
    raw
      .split(/\r?\n/)
      .map((l) => l.trim())
      .find(Boolean) ?? "";
  const cleaned = line
    .replace(/[\\/:*?"<>|\u0000-\u001f]/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 80)
    .replace(/[.\s]+$/, "")
    .trim();
  const base = cleaned && cleaned !== "." ? cleaned : "Заметка";
  return base.toLowerCase().endsWith(".md") ? base : `${base}.md`;
}

// notes_create refuses to overwrite, so pick a free name in the vault root.
export async function uniqueNoteRel(base: string): Promise<string> {
  const taken = new Set((await ipc.notesList()).map((e) => e.name));
  if (!taken.has(base)) return base;
  const dot = base.lastIndexOf(".");
  const stem = dot > 0 ? base.slice(0, dot) : base;
  const ext = dot > 0 ? base.slice(dot) : "";
  let n = 2;
  while (taken.has(`${stem} (${n})${ext}`)) n++;
  return `${stem} (${n})${ext}`;
}

// Create a Notes/ note with a collision-safe name; returns the vault path.
export async function createNote(name: string, content: string): Promise<string> {
  const rel = await uniqueNoteRel(name);
  await ipc.notesCreate(rel, content);
  return rel;
}

// Hover quick action for project files (tree rows, grid tiles, preview head):
// «Спросить агента» routes a prebuilt question to the project chat.
export function useFileCrossActions(projectId: number) {
  const actions = useAppActions();
  const askAgent = (entry: FsEntry) =>
    actions.requestAskAgent(
      projectId,
      `Посмотри файл «${entry.relPath}» и коротко расскажи, что в нём.`,
    );
  return { askAgent };
}

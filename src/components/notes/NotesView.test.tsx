import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const notesList = vi.fn();
vi.mock("../../api/ipc", () => ({
  notesEnsure: vi.fn(async () => {}),
  onNotesChanged: vi.fn(async () => () => {}),
  onIndexChanged: vi.fn(async () => () => {}),
  notesList: (...args: unknown[]) => notesList(...args),
}));

const toast = vi.fn();
vi.mock("../shared/toast", () => ({ toast: (...args: unknown[]) => toast(...args) }));

vi.mock("./NoteEditor", () => ({ default: () => null }));
vi.mock("./NotesChat", () => ({ default: () => null }));
vi.mock("./NotesTree", () => ({ default: () => null }));
vi.mock("./NotesSearchResults", () => ({ default: () => null }));

let pendingPath: string | null = null;
vi.mock("../../state/AppStore", () => {
  const actions = {
    clearPendingNote: () => {},
    clearNewNote: () => {},
    requestNote: () => {},
  };
  return {
    useApp: () => ({
      state: { pendingNotePath: pendingPath, newNoteRequested: false },
      actions,
    }),
  };
});

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import NotesView from "./NotesView";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  pendingPath = null;
  vi.clearAllMocks();
});

describe("NotesView pending-note intent", () => {
  it("shows an error toast when the requested note is missing", async () => {
    pendingPath = "missing.md";
    notesList.mockResolvedValue([]);
    await act(async () => {
      root.render(<NotesView />);
    });
    expect(toast).toHaveBeenCalledWith("err", expect.stringContaining("missing.md"));
  });

  it("opens a found note without an error toast", async () => {
    pendingPath = "found.md";
    notesList.mockResolvedValue([{ relPath: "found.md", name: "found.md", kind: "file" }]);
    await act(async () => {
      root.render(<NotesView />);
    });
    expect(toast).not.toHaveBeenCalled();
  });
});

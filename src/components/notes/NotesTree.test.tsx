import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { FsEntry } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const dir = (name: string, relPath: string): FsEntry => ({
  name,
  relPath,
  kind: "dir",
  ext: "",
  size: 0,
  modified: 0,
  isMarkdown: false,
  isText: false,
  isImage: false,
});

const file = (name: string, relPath: string): FsEntry => ({
  name,
  relPath,
  kind: "file",
  ext: "md",
  size: 1,
  modified: 0,
  isMarkdown: true,
  isText: true,
  isImage: false,
});

// notes_tree returns the whole vault as one flat list; the fixture is mutated
// per test to simulate what the watcher/notes-changed bump reveals after an
// import.
const entries = vi.hoisted((): FsEntry[] => []);

const notesTree = vi.hoisted(() => vi.fn(async () => entries.slice()));
const notesImport = vi.hoisted(() => vi.fn(async (_destRel: string, _paths: string[]) => []));

vi.mock("../../api/ipc", () => ({
  notesTree,
  notesImport,
  notesMove: vi.fn(async () => undefined),
  notesRename: vi.fn(async () => undefined),
  notesDelete: vi.fn(async () => undefined),
  notesCreate: vi.fn(async () => undefined),
  templatesList: vi.fn(async () => ({ notes: [] })),
  templateCreateNote: vi.fn(async () => undefined),
}));

// Captures the webview drag-drop callback so tests can fire OS (Finder)
// enter/over/drop events like useOsFileDrag receives them in Tauri.
const osDragHandler = vi.hoisted(
  () => ({
    current: null as null | ((event: { payload: Record<string, unknown> }) => void),
  }),
);

vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: vi.fn(async (cb: (event: { payload: Record<string, unknown> }) => void) => {
      osDragHandler.current = cb;
      return () => {
        osDragHandler.current = null;
      };
    }),
  }),
}));

import NotesTree from "./NotesTree";

let container: HTMLDivElement;
let root: Root;
// Element reported by the mocked document.elementFromPoint during an OS drag.
let hoverEl: Element | null;

beforeEach(() => {
  hoverEl = null;
  notesTree.mockClear();
  notesImport.mockClear();
  entries.length = 0;
  entries.push(dir("docs", "docs"), file("a.md", "a.md"), file("inner.md", "docs/inner.md"));
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function mount(treeVersion = 0, selectedDirRel = "") {
  document.elementFromPoint = vi.fn(() => hoverEl) as unknown as typeof document.elementFromPoint;
  await act(async () => {
    root.render(
      <NotesTree
        treeVersion={treeVersion}
        selectedRelPath={null}
        selectedDirRel={selectedDirRel}
        onSelect={vi.fn()}
        onDeleted={vi.fn()}
        onRenamed={vi.fn()}
        onMoved={vi.fn()}
      />,
    );
  });
  await settle();
}

const settle = async () => {
  await act(async () => {
    await Promise.resolve();
  });
};

const osDrag = (type: string, paths: string[] = []) =>
  act(async () => {
    osDragHandler.current?.({ payload: { type, position: { x: 5, y: 5 }, paths } });
  });

const row = (name: string) =>
  Array.from(container.querySelectorAll<HTMLElement>(".tree-row")).find(
    (r) => r.querySelector(".label")?.textContent === name,
  )!;

describe("NotesTree Finder drag import (OS drag-drop)", () => {
  it("OS drag over a folder row highlights it; drop imports into that folder", async () => {
    await mount();
    hoverEl = row("docs");
    await osDrag("over");
    expect(row("docs").className).toContain("drop-over");
    await osDrag("drop", ["/tmp/photo.png"]);
    expect(notesImport).toHaveBeenCalledWith("docs", ["/tmp/photo.png"]);
    expect(row("docs").className).not.toContain("drop-over");
  });

  it("OS drag over the tree root imports into the vault root", async () => {
    await mount();
    hoverEl = container.querySelector(".notes-tree");
    await osDrag("over");
    expect(container.querySelector(".notes-tree")?.className).toContain("drop-over");
    await osDrag("drop", ["/tmp/нота.md"]);
    expect(notesImport).toHaveBeenCalledWith("", ["/tmp/нота.md"]);
  });

  it("drop outside any drop zone falls back to the selected folder", async () => {
    await mount(0, "docs");
    hoverEl = null;
    await osDrag("drop", ["/tmp/a.txt"]);
    expect(notesImport).toHaveBeenCalledWith("docs", ["/tmp/a.txt"]);
  });

  it("drop over the chat attachment zone is ignored by the tree", async () => {
    await mount();
    const chatZone = document.createElement("div");
    chatZone.setAttribute("data-chat-drop", "");
    container.appendChild(chatZone);
    hoverEl = chatZone;
    await osDrag("over");
    await osDrag("drop", ["/tmp/a.txt"]);
    expect(notesImport).not.toHaveBeenCalled();
  });

  it("a failed import shows the existing error style", async () => {
    notesImport.mockRejectedValueOnce(new Error('"x" escapes the Notes folder'));
    await mount();
    hoverEl = container.querySelector(".notes-tree");
    await osDrag("drop", ["/tmp/a.txt"]);
    expect(container.querySelector(".tree-error")?.textContent).toContain("escapes the Notes folder");
  });

  it("the tree refreshes when treeVersion bumps after an import (notes-changed)", async () => {
    await mount();
    hoverEl = container.querySelector(".notes-tree");
    await osDrag("drop", ["/tmp/new.md"]);
    expect(notesImport).toHaveBeenCalledWith("", ["/tmp/new.md"]);
    // The backend emits notes-changed; NotesView bumps treeVersion, the new
    // vault listing carries the imported file.
    entries.push(file("new.md", "new.md"));
    await mount(1);
    expect(notesTree).toHaveBeenCalledTimes(2);
    expect(row("new.md")).toBeDefined();
  });
});

describe("NotesTree non-md files and labels", () => {
  const anyFile = (name: string, relPath: string, over: Partial<FsEntry>): FsEntry => ({
    name,
    relPath,
    kind: "file",
    ext: "",
    size: 10,
    modified: 0,
    isMarkdown: false,
    isText: false,
    isImage: false,
    ...over,
  });

  it("lists images, PDFs and archives with ext tags and entry icons", async () => {
    entries.push(
      anyFile("pic.png", "pic.png", { ext: "png", isImage: true }),
      anyFile("doc.pdf", "doc.pdf", { ext: "pdf" }),
      anyFile("archive.zip", "archive.zip", { ext: "zip" }),
    );
    await mount();
    for (const name of ["pic.png", "doc.pdf", "archive.zip"]) {
      expect(row(name)).toBeDefined();
    }
    expect(row("pic.png").querySelector(".tag")?.textContent).toBe("png");
    expect(row("pic.png").querySelector(".icon svg")).toBeTruthy();
    expect(row("archive.zip").querySelector(".icon svg")).toBeTruthy();
    expect(row("docs").querySelector(".icon.folder-icon svg")).toBeTruthy();
  });

  it("the empty state mentions files too", async () => {
    entries.length = 0;
    await mount();
    expect(container.querySelector(".empty-note")?.textContent).toBe(
      "Заметок и файлов пока нет",
    );
  });
});

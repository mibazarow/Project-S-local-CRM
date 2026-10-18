import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from "vitest";
import type { FsEntry } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const tree = vi.hoisted((): Record<string, FsEntry[]> => ({
  "": [
    {
      name: "docs",
      relPath: "docs",
      kind: "dir",
      ext: "",
      size: 0,
      modified: 0,
      isMarkdown: false,
      isText: false,
      isImage: false,
    },
    {
      name: "a.txt",
      relPath: "a.txt",
      kind: "file",
      ext: "txt",
      size: 1,
      modified: 0,
      isMarkdown: false,
      isText: true,
      isImage: false,
    },
  ],
  docs: [
    {
      name: "inner.md",
      relPath: "docs/inner.md",
      kind: "file",
      ext: "md",
      size: 1,
      modified: 0,
      isMarkdown: true,
      isText: true,
      isImage: false,
    },
  ],
}));

const fsMove = vi.hoisted(
  () =>
    vi.fn(async (_projectId: number, fromRel: string, toDirRel: string): Promise<FsEntry> => {
      const name = fromRel.slice(fromRel.lastIndexOf("/") + 1);
      return {
        name,
        relPath: toDirRel ? `${toDirRel}/${name}` : name,
        kind: "file",
        ext: name.includes(".") ? name.slice(name.lastIndexOf(".") + 1) : "",
        size: 1,
        modified: 0,
        isMarkdown: false,
        isText: true,
        isImage: false,
      };
    }),
);

vi.mock("../../api/ipc", () => ({
  listProjects: vi.fn(async () => []),
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-test" })),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async () => () => {}),
  onTasksChanged: vi.fn(async () => () => {}),
  onSchedulesChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
  fsListDir: vi.fn(async (_projectId: number, relPath: string) => tree[relPath] ?? []),
  fsMove,
  fsRename: vi.fn(async () => undefined),
  fsDelete: vi.fn(async () => undefined),
  fsCreateFolder: vi.fn(async () => undefined),
  fsCreateFile: vi.fn(async () => undefined),
  fsImport: vi.fn(async () => []),
  fsOpenExternal: vi.fn(async () => undefined),
}));

vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: vi.fn(async () => () => {}),
  }),
}));

import FileTree from "./FileTree";
import { AppProvider } from "../../state/AppStore";

let container: HTMLDivElement;
let root: Root;
let onSelect: Mock<(entry: FsEntry) => void>;
// Element reported by the mocked document.elementFromPoint during a drag.
let hoverEl: Element | null;

beforeEach(() => {
  onSelect = vi.fn<(entry: FsEntry) => void>();
  hoverEl = null;
  fsMove.mockClear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function mount(selectedRelPath: string | null = null) {
  document.elementFromPoint = vi.fn(() => hoverEl) as unknown as typeof document.elementFromPoint;
  await act(async () => {
    root.render(
      <AppProvider>
        <FileTree
          projectId={1}
          treeVersion={0}
          selectedRelPath={selectedRelPath}
          selectedDirRel=""
          onSelect={onSelect}
          view="list"
          onViewChange={() => {}}
        />
      </AppProvider>,
    );
  });
  await settle();
}

const settle = async () => {
  await act(async () => {
    await Promise.resolve();
  });
};

const fire = (target: EventTarget, type: string, x = 0, y = 0) =>
  act(() => {
    target.dispatchEvent(
      new MouseEvent(type, {
        bubbles: true,
        cancelable: true,
        button: 0,
        clientX: x,
        clientY: y,
      }),
    );
  });

const row = (name: string) =>
  Array.from(container.querySelectorAll<HTMLElement>(".tree-row")).find(
    (r) => r.querySelector(".label")?.textContent === name,
  )!;

// Drag `srcName`'s row and release while elementFromPoint reports `destEl`.
async function dragTo(srcName: string, destEl: Element | null) {
  hoverEl = destEl;
  fire(row(srcName), "pointerdown", 10, 10);
  fire(window, "pointermove", 10, 60);
  fire(window, "pointermove", 10, 100);
  fire(window, "pointerup", 10, 100);
  await settle();
}

describe("FileTree pointer drag (list mode)", () => {
  it("dragging a file row onto a folder row moves it via fs_move", async () => {
    await mount();
    await dragTo("a.txt", row("docs"));
    expect(fsMove).toHaveBeenCalledWith(1, "a.txt", "docs");
  });

  it("dragging a file from an open folder to the tree root moves it to the project root", async () => {
    await mount();
    await act(async () => {
      row("docs").dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    await settle();
    expect(row("inner.md")).toBeDefined();
    await dragTo("inner.md", container.querySelector(".file-tree"));
    expect(fsMove).toHaveBeenCalledWith(1, "docs/inner.md", "");
  });

  it("valid folder target is highlighted while dragging; the dragged row dims", async () => {
    await mount();
    hoverEl = row("docs");
    fire(row("a.txt"), "pointerdown", 10, 10);
    fire(window, "pointermove", 10, 60);
    fire(window, "pointermove", 10, 100);
    expect(row("docs").className).toContain("drop-over");
    expect(row("a.txt").className).toContain("dragging");
    expect(document.body.classList.contains("dnd-active")).toBe(true);
    fire(window, "pointerup", 10, 100);
    await settle();
    expect(fsMove).toHaveBeenCalledWith(1, "a.txt", "docs");
  });

  it("dropping a file into its current parent folder is a no-op", async () => {
    await mount();
    await act(async () => {
      row("docs").dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    await settle();
    await dragTo("inner.md", row("docs"));
    expect(fsMove).not.toHaveBeenCalled();
  });

  it("Escape cancels the drag: no move, highlights cleared", async () => {
    await mount();
    hoverEl = row("docs");
    fire(row("a.txt"), "pointerdown", 10, 10);
    fire(window, "pointermove", 10, 60);
    fire(window, "pointermove", 10, 100);
    expect(row("docs").className).toContain("drop-over");
    act(() => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    fire(window, "pointerup", 10, 100);
    await settle();
    expect(fsMove).not.toHaveBeenCalled();
    expect(row("docs").className).not.toContain("drop-over");
    expect(row("a.txt").className).not.toContain("dragging");
  });

  it("pointercancel cancels the drag: no move", async () => {
    await mount();
    hoverEl = row("docs");
    fire(row("a.txt"), "pointerdown", 10, 10);
    fire(window, "pointermove", 10, 60);
    fire(window, "pointermove", 10, 100);
    fire(window, "pointercancel", 10, 100);
    await settle();
    expect(fsMove).not.toHaveBeenCalled();
  });

  it("folder rows do not start a pointer drag (files only in this slice)", async () => {
    await mount();
    await dragTo("docs", container.querySelector(".file-tree"));
    expect(fsMove).not.toHaveBeenCalled();
  });

  it("a plain click still selects the file row", async () => {
    await mount();
    fire(row("a.txt"), "pointerdown", 10, 10);
    fire(window, "pointerup", 10, 10);
    fire(row("a.txt"), "click", 10, 10);
    await settle();
    expect(onSelect).toHaveBeenCalledWith(expect.objectContaining({ relPath: "a.txt" }));
    expect(fsMove).not.toHaveBeenCalled();
  });

  it("the click after a drag does not select the row", async () => {
    await mount();
    hoverEl = null;
    fire(row("a.txt"), "pointerdown", 10, 10);
    fire(window, "pointermove", 10, 60);
    fire(window, "pointerup", 10, 60);
    fire(row("a.txt"), "click", 10, 60);
    await settle();
    expect(onSelect).not.toHaveBeenCalled();
    expect(fsMove).not.toHaveBeenCalled();
  });

  it("selection follows the moved file to its new path", async () => {
    await mount("a.txt");
    await dragTo("a.txt", row("docs"));
    expect(fsMove).toHaveBeenCalledWith(1, "a.txt", "docs");
    expect(onSelect).toHaveBeenCalledWith(expect.objectContaining({ relPath: "docs/a.txt" }));
  });
});

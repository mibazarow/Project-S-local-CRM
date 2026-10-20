import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from "vitest";
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

const file = (name: string, relPath: string, ext: string, isMarkdown = false): FsEntry => ({
  name,
  relPath,
  kind: "file",
  ext,
  size: 1,
  modified: 0,
  isMarkdown,
  isText: true,
  isImage: false,
});

// Fresh fixture per test: the fsMove/fsCreateFolder mocks mutate `tree`, so
// tests must not leak listings into each other.
const initialTree = vi.hoisted(
  () =>
    (): Record<string, FsEntry[]> => ({
      "": [dir("docs", "docs"), dir("src", "src"), file("a.txt", "a.txt", "txt")],
      docs: [file("inner.md", "docs/inner.md", "md", true), dir("sub", "docs/sub")],
      "docs/sub": [],
      src: [],
    }),
);

const tree = vi.hoisted((): Record<string, FsEntry[]> => ({}));

const fsListDir = vi.hoisted(
  () => vi.fn(async (_projectId: number, relPath: string) => tree[relPath] ?? []),
);

// Mirrors fs_move: relocates the entry in the fixture, renumbering the whole
// subtree, and returns the moved entry like the backend does.
const fsMove = vi.hoisted(
  () =>
    vi.fn(async (_projectId: number, fromRel: string, toDirRel: string): Promise<FsEntry> => {
      const name = fromRel.slice(fromRel.lastIndexOf("/") + 1);
      const newRel = toDirRel ? `${toDirRel}/${name}` : name;
      let moved: FsEntry | undefined;
      for (const kids of Object.values(tree)) {
        moved = kids.find((e) => e.relPath === fromRel);
        if (moved) break;
      }
      if (!moved) throw new Error(`no such entry: ${fromRel}`);
      for (const kids of Object.values(tree)) {
        for (let i = kids.length - 1; i >= 0; i--) {
          if (kids[i].relPath === fromRel) kids.splice(i, 1);
        }
      }
      if (tree[fromRel]) {
        tree[newRel] = tree[fromRel];
        delete tree[fromRel];
      }
      const prefix = `${fromRel}/`;
      const remap = (e: FsEntry): FsEntry =>
        e.relPath === fromRel
          ? { ...e, relPath: newRel }
          : e.relPath.startsWith(prefix)
            ? { ...e, relPath: newRel + e.relPath.slice(fromRel.length) }
            : e;
      for (const kids of Object.values(tree)) {
        for (let i = 0; i < kids.length; i++) kids[i] = remap(kids[i]);
      }
      const movedEntry = remap(moved);
      (tree[toDirRel] ??= []).push(movedEntry);
      return movedEntry;
    }),
);

const fsImport = vi.hoisted(() => vi.fn(async () => [] as FsEntry[]));

const fsCreateFolder = vi.hoisted(
  () =>
    vi.fn(async (_projectId: number, parentRel: string, name: string): Promise<FsEntry> => {
      const relPath = parentRel ? `${parentRel}/${name}` : name;
      const entry = dir(name, relPath);
      (tree[parentRel] ??= []).push(entry);
      tree[relPath] ??= [];
      return entry;
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
  onScheduleOpen: vi.fn(async () => () => {}),
  fsListDir,
  fsMove,
  fsRename: vi.fn(async () => undefined),
  fsDelete: vi.fn(async () => undefined),
  fsCreateFolder,
  fsCreateFile: vi.fn(async () => undefined),
  fsImport,
  fsOpenExternal: vi.fn(async () => undefined),
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
  fsListDir.mockClear();
  fsCreateFolder.mockClear();
  fsImport.mockClear();
  for (const key of Object.keys(tree)) delete tree[key];
  Object.assign(tree, initialTree());
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function mount(selectedRelPath: string | null = null, selectedDirRel = "") {
  document.elementFromPoint = vi.fn(() => hoverEl) as unknown as typeof document.elementFromPoint;
  await act(async () => {
    root.render(
      <AppProvider>
        <FileTree
          projectId={1}
          treeVersion={0}
          selectedRelPath={selectedRelPath}
          selectedDirRel={selectedDirRel}
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

async function openRow(name: string) {
  await act(async () => {
    row(name).dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
  await settle();
}

// Drag `srcName`'s row and release while elementFromPoint reports `destEl`.
async function dragTo(srcName: string, destEl: Element | null) {
  hoverEl = destEl;
  fire(row(srcName), "pointerdown", 10, 10);
  fire(window, "pointermove", 10, 60);
  fire(window, "pointermove", 10, 100);
  fire(window, "pointerup", 10, 100);
  await settle();
}

const toolbarBtn = (label: string) =>
  Array.from(container.querySelectorAll<HTMLButtonElement>(".tree-toolbar button")).find(
    (b) => b.textContent?.includes(label),
  )!;

async function clickToolbar(label: string) {
  await act(async () => {
    toolbarBtn(label).click();
  });
  await settle();
}

async function commitCreateInput(name: string) {
  const input = container.querySelector<HTMLInputElement>(".rename-input")!;
  await act(async () => {
    input.value = name;
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  });
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
    await openRow("docs");
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
    await openRow("docs");
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

describe("FileTree folder drag (list mode)", () => {
  it("dragging a folder row into another folder moves it via fs_move", async () => {
    await mount();
    await dragTo("docs", row("src"));
    expect(fsMove).toHaveBeenCalledWith(1, "docs", "src");
  });

  it("dragging an open subfolder to the tree root moves it to the project root", async () => {
    await mount();
    await openRow("docs");
    await dragTo("sub", container.querySelector(".file-tree"));
    expect(fsMove).toHaveBeenCalledWith(1, "docs/sub", "");
  });

  it("dropping a folder on itself or its own subfolder is rejected", async () => {
    await mount();
    await openRow("docs");
    await dragTo("docs", row("docs"));
    await dragTo("docs", row("sub"));
    expect(fsMove).not.toHaveBeenCalled();
  });

  it("dropping a folder into its current parent is a no-op", async () => {
    await mount();
    await openRow("docs");
    await dragTo("sub", row("docs"));
    expect(fsMove).not.toHaveBeenCalled();
  });

  it("an open subtree stays expanded under the moved folder's new path", async () => {
    await mount();
    await openRow("src");
    await openRow("docs");
    await openRow("sub");
    await dragTo("docs", row("src"));
    expect(fsMove).toHaveBeenCalledWith(1, "docs", "src");
    // The remapped open paths are relisted at their new locations.
    expect(fsListDir).toHaveBeenCalledWith(1, "src/docs");
    expect(fsListDir).toHaveBeenCalledWith(1, "src/docs/sub");
    expect(row("sub")).toBeDefined();
  });

  it("selection follows the moved folder to its new path", async () => {
    await mount("docs");
    await dragTo("docs", row("src"));
    expect(onSelect).toHaveBeenCalledWith(expect.objectContaining({ relPath: "src/docs" }));
  });

  it("selection follows a file inside the moved subtree", async () => {
    await mount("docs/inner.md");
    await dragTo("docs", row("src"));
    expect(onSelect).toHaveBeenCalledWith(
      expect.objectContaining({ relPath: "src/docs/inner.md" }),
    );
  });

  it("Escape cancels a folder drag: no move", async () => {
    await mount();
    hoverEl = row("src");
    fire(row("docs"), "pointerdown", 10, 10);
    fire(window, "pointermove", 10, 60);
    fire(window, "pointermove", 10, 100);
    act(() => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    fire(window, "pointerup", 10, 100);
    await settle();
    expect(fsMove).not.toHaveBeenCalled();
  });
});

describe("FileTree subfolder creation (list mode)", () => {
  it("creates a subfolder in the selected folder, opens it and lists it", async () => {
    await mount(null, "docs");
    await clickToolbar("Новая папка");
    expect(container.querySelector(".rename-input")).not.toBeNull();
    await commitCreateInput("new");
    expect(fsCreateFolder).toHaveBeenCalledWith(1, "docs", "new");
    expect(fsListDir).toHaveBeenCalledWith(1, "docs/new");
    expect(row("new")).toBeDefined();
  });

  it("creates in the project root when no folder is selected", async () => {
    await mount();
    await clickToolbar("Новая папка");
    await commitCreateInput("rootdir");
    expect(fsCreateFolder).toHaveBeenCalledWith(1, "", "rootdir");
    expect(row("rootdir")).toBeDefined();
  });

  it("an invalid name shows the existing error style", async () => {
    fsCreateFolder.mockRejectedValueOnce(new Error('invalid name "a/b"'));
    await mount();
    await clickToolbar("Новая папка");
    await commitCreateInput("a/b");
    expect(container.querySelector(".tree-error")?.textContent).toContain("invalid name");
  });

  it("a duplicate name shows the existing error style", async () => {
    fsCreateFolder.mockRejectedValueOnce(new Error('"docs" already exists'));
    await mount();
    await clickToolbar("Новая папка");
    await commitCreateInput("docs");
    expect(container.querySelector(".tree-error")?.textContent).toContain("already exists");
  });

  it("Escape in the name prompt cancels creation", async () => {
    await mount();
    await clickToolbar("Новая папка");
    const input = container.querySelector<HTMLInputElement>(".rename-input")!;
    await act(async () => {
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    await settle();
    expect(fsCreateFolder).not.toHaveBeenCalled();
    expect(container.querySelector(".rename-input")).toBeNull();
  });

  it("the folder context menu creates a subfolder inside that folder", async () => {
    await mount();
    await act(async () => {
      row("docs").dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, cancelable: true }),
      );
    });
    const item = Array.from(
      document.querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).find((b) => b.textContent?.includes("Новая папка"))!;
    expect(item).toBeDefined();
    await act(async () => {
      item.click();
    });
    await settle();
    expect(container.querySelector(".rename-input")).not.toBeNull();
    await commitCreateInput("sub2");
    expect(fsCreateFolder).toHaveBeenCalledWith(1, "docs", "sub2");
  });
});

describe("FileTree Finder drag import (OS drag-drop)", () => {
  const osDrag = (type: string, paths: string[] = []) =>
    act(async () => {
      osDragHandler.current?.({ payload: { type, position: { x: 5, y: 5 }, paths } });
    });

  it("OS drag over a folder row highlights it; drop imports into that folder", async () => {
    await mount();
    hoverEl = row("docs");
    await osDrag("over");
    expect(row("docs").className).toContain("drop-over");
    await osDrag("drop", ["/tmp/photo.png"]);
    expect(fsImport).toHaveBeenCalledWith(1, "docs", ["/tmp/photo.png"]);
    expect(row("docs").className).not.toContain("drop-over");
  });

  it("OS drag over the tree root imports into the project root", async () => {
    await mount();
    hoverEl = container.querySelector(".file-tree");
    await osDrag("over");
    expect(container.querySelector(".file-tree")?.className).toContain("drop-over");
    await osDrag("drop", ["/tmp/a.txt"]);
    expect(fsImport).toHaveBeenCalledWith(1, "", ["/tmp/a.txt"]);
  });

  it("drop over the chat attachment zone is ignored by the tree", async () => {
    await mount();
    const chatZone = document.createElement("div");
    chatZone.setAttribute("data-chat-drop", "");
    container.appendChild(chatZone);
    hoverEl = chatZone;
    await osDrag("over");
    await osDrag("drop", ["/tmp/a.txt"]);
    expect(fsImport).not.toHaveBeenCalled();
  });

  it("drop outside any drop zone falls back to the selected folder", async () => {
    await mount(null, "docs");
    hoverEl = null;
    await osDrag("drop", ["/tmp/a.txt"]);
    expect(fsImport).toHaveBeenCalledWith(1, "docs", ["/tmp/a.txt"]);
  });

  it("a failed import shows the existing error style", async () => {
    fsImport.mockRejectedValueOnce(new Error('"x" is not a folder'));
    await mount();
    hoverEl = row("a.txt");
    await osDrag("drop", ["/tmp/a.txt"]);
    expect(container.querySelector(".tree-error")?.textContent).toContain("is not a folder");
  });
});

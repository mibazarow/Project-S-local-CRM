import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { FsEntry, ProjectDto } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const entries = vi.hoisted(
  (): FsEntry[] => [
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
);

const projects = vi.hoisted(() => [
  {
    id: 1,
    name: "Alpha",
    path: "/p/alpha",
    createdAt: "2026-01-01T00:00:00Z",
    openedAt: null,
    position: 0,
    fileCount: 1,
    taskCount: 0,
    overdueCount: 0,
    hasActiveTasks: false,
    taskColumns: ["a", "b", "c"],
    filesView: "list",
  } satisfies ProjectDto,
  {
    id: 2,
    name: "Beta",
    path: "/p/beta",
    createdAt: "2026-01-01T00:00:00Z",
    openedAt: null,
    position: 1,
    fileCount: 1,
    taskCount: 0,
    overdueCount: 0,
    hasActiveTasks: false,
    taskColumns: ["a", "b", "c"],
    filesView: "grid",
  } satisfies ProjectDto,
]);

vi.mock("../../api/ipc", () => ({
  listProjects: vi.fn(async () => projects),
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-test" })),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async () => () => {}),
  onTasksChanged: vi.fn(async () => () => {}),
  onSchedulesChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
  fsListDir: vi.fn(async () => entries),
  tasksList: vi.fn(async () => []),
  projectSetFilesView: vi.fn(async () => undefined),
}));

vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: vi.fn(async () => () => {}),
  }),
}));

import * as ipc from "../../api/ipc";
import FilesPanel from "./FilesPanel";
import { AppProvider } from "../../state/AppStore";

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.mocked(ipc.projectSetFilesView).mockClear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function mount(project: ProjectDto) {
  await act(async () => {
    root.render(
      <AppProvider>
        <FilesPanel key={project.id} project={project} />
      </AppProvider>,
    );
  });
  await act(async () => {
    await Promise.resolve();
  });
}

// App.tsx keys FilesPanel by project id, so switching projects remounts it.
async function switchTo(project: ProjectDto) {
  await act(async () => {
    root.render(
      <AppProvider>
        <FilesPanel key={project.id} project={project} />
      </AppProvider>,
    );
  });
  await act(async () => {
    await Promise.resolve();
  });
}

const segs = () => Array.from(container.querySelectorAll(".center-head .seg"));
const vts = () => Array.from(container.querySelectorAll(".view-toggle .vt"));
const click = (el: Element) =>
  act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });

describe("FilesPanel segmented tab switch", () => {
  it("renders files/tasks segments with files active and the search box shown", async () => {
    await mount(projects[0]);
    expect(segs()).toHaveLength(2);
    expect(segs()[0].className).toBe("seg active");
    expect(segs()[0].textContent).toBe("Файлы");
    expect(segs()[1].className).toBe("seg");
    expect(segs()[1].textContent).toBe("Задачи");
    expect(container.querySelector(".center-head .search input")).not.toBeNull();
  });

  it("clicking the tasks segment activates it, hides the search box and renders the board", async () => {
    await mount(projects[0]);
    await click(segs()[1]);
    expect(segs()[0].className).toBe("seg");
    expect(segs()[1].className).toBe("seg active");
    expect(container.querySelector(".center-head .search input")).toBeNull();
    expect(container.querySelector(".task-filters")).not.toBeNull();
  });
});

describe("FilesPanel per-project file view mode", () => {
  it("hydrates the view from project.filesView", async () => {
    await mount(projects[0]);
    expect(container.querySelector(".file-tree")).not.toBeNull();
    expect(container.querySelector(".file-grid")).toBeNull();
    expect(vts()[0].className).toBe("vt active");
    expect(vts()[1].className).toBe("vt");

    await mount(projects[1]);
    expect(container.querySelector(".file-grid")).not.toBeNull();
    expect(container.querySelector(".file-tree")).toBeNull();
    expect(vts()[0].className).toBe("vt");
    expect(vts()[1].className).toBe("vt active");
  });

  it("toggling flips the DOM immediately and persists via IPC", async () => {
    await mount(projects[0]);
    await click(vts()[1]);
    expect(container.querySelector(".file-grid")).not.toBeNull();
    expect(container.querySelector(".file-tree")).toBeNull();
    expect(vi.mocked(ipc.projectSetFilesView)).toHaveBeenCalledWith(1, "grid");
  });

  it("switching projects keeps each project's mode without resetting to list", async () => {
    await mount(projects[0]);
    expect(container.querySelector(".file-tree")).not.toBeNull();
    await switchTo(projects[1]);
    expect(container.querySelector(".file-grid")).not.toBeNull();
    await switchTo(projects[0]);
    expect(container.querySelector(".file-tree")).not.toBeNull();
    expect(vi.mocked(ipc.projectSetFilesView)).not.toHaveBeenCalled();
  });

  it("keeps the persisted mode after a restart (store rehydrate)", async () => {
    await mount(projects[0]);
    await click(vts()[1]);
    expect(vi.mocked(ipc.projectSetFilesView)).toHaveBeenCalledWith(1, "grid");
    // The backend persisted the mode: a fresh boot hands the panel the
    // reloaded DTO, and the panel opens in grid without any click.
    projects[0].filesView = "grid";
    try {
      act(() => root.unmount());
      root = createRoot(container);
      await mount(projects[0]);
      expect(container.querySelector(".file-grid")).not.toBeNull();
      expect(vts()[1].className).toBe("vt active");
    } finally {
      projects[0].filesView = "list";
    }
  });

  it("keeps the list/grid DOM and the shared view-toggle markup", async () => {
    await mount(projects[0]);
    expect(container.querySelector(".tree-toolbar .view-toggle[title='Вид файлов']")).not.toBeNull();
    expect(vts()).toHaveLength(2);
    expect(container.querySelector(".file-tree .tree-children.root")).not.toBeNull();
    await click(vts()[1]);
    expect(container.querySelector(".tree-toolbar .view-toggle[title='Вид файлов']")).not.toBeNull();
    expect(vts()).toHaveLength(2);
    expect(container.querySelector(".file-grid")).not.toBeNull();
  });
});

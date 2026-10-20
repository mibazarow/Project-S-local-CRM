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
      name: "report.pdf",
      relPath: "report.pdf",
      kind: "file",
      ext: "pdf",
      size: 1,
      modified: 0,
      isMarkdown: false,
      isText: false,
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
    archived: false,
    archivedAt: null,
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
    archived: false,
    archivedAt: null,
  } satisfies ProjectDto,
]);

vi.mock("../../api/ipc", () => ({
  listProjects: vi.fn(async () => projects),
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-test" })),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async () => () => {}),
  onIndexChanged: vi.fn(async () => () => {}),
  onTasksChanged: vi.fn(async () => () => {}),
  onSchedulesChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
  onScheduleOpen: vi.fn(async () => () => {}),
  fsListDir: vi.fn(async (_projectId: number, dir: string) => (dir === "" ? entries : [])),
  fsMove: vi.fn(async () => entries[0]),
  fsReadText: vi.fn(async () => "hello"),
  fsWriteText: vi.fn(async () => entries[0]),
  fsReadBinaryDataUrl: vi.fn(async () => ""),
  projectPath: vi.fn(async () => "/p/alpha"),
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
import { AppProvider, useApp } from "../../state/AppStore";

function RootNavTrigger({ projectId }: { projectId: number }) {
  const { actions } = useApp();
  return <button className="test-root-nav" onClick={() => actions.requestProjectRoot(projectId)} />;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.mocked(ipc.projectSetFilesView).mockClear();
  vi.mocked(ipc.fsMove).mockClear();
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
  it("renders files/tasks segments with files active", async () => {
    await mount(projects[0]);
    expect(segs()).toHaveLength(2);
    expect(segs()[0].className).toBe("seg active");
    expect(segs()[0].textContent).toBe("Файлы");
    expect(segs()[1].className).toBe("seg");
    expect(segs()[1].textContent).toBe("Задачи");
  });

  it("clicking the tasks segment activates it and renders the board", async () => {
    await mount(projects[0]);
    expect(container.querySelector(".seg-filter")).toBeNull();
    await click(segs()[1]);
    expect(segs()[0].className).toBe("seg");
    expect(segs()[1].className).toBe("seg active");
    expect(container.querySelector(".seg-filter")).not.toBeNull();
  });

  it("toggles the overdue filter and its active styling", async () => {
    await mount(projects[0]);
    await click(segs()[1]);
    const filter = container.querySelector(".seg-filter")!;
    const input = filter.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
    expect(input.checked).toBe(false);
    expect(filter.className).toBe("seg-filter");

    await act(async () => {
      input.click();
    });
    expect(input.checked).toBe(true);
    expect(filter.className).toBe("seg-filter active");

    await act(async () => {
      input.click();
    });
    expect(input.checked).toBe(false);
    expect(filter.className).toBe("seg-filter");
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

  it("grid tiles label the document action «Открыть в редакторе»", async () => {
    await mount(projects[0]);
    await click(vts()[1]);
    const tile = Array.from(container.querySelectorAll<HTMLElement>(".tile")).find(
      (t) => t.querySelector(".label")?.textContent === "report.pdf",
    )!;
    expect(tile.querySelector('button[title="Открыть в редакторе"]')).not.toBeNull();
    expect(tile.querySelector('button[title="Открыть с помощью…"]')).toBeNull();
  });
});

describe("FilesPanel archived project (Q.md item 5)", () => {
  const archivedProject = (p: ProjectDto): ProjectDto => ({
    ...p,
    archived: true,
    archivedAt: "2026-01-01 10:00:00",
  });

  it("renders no Tasks tab and no board for an archived project", async () => {
    await mount(archivedProject(projects[0]));
    expect(segs()).toHaveLength(1);
    expect(segs()[0].textContent).toBe("Файлы");
    expect(segs()[0].className).toBe("seg active");
    expect(container.querySelector(".task-board")).toBeNull();
    expect(container.querySelector(".task-add-btn")).toBeNull();
  });

  it("archiving a project with the Tasks tab open switches back to Файлы", async () => {
    await mount(projects[0]);
    await click(segs()[1]);
    expect(segs()[1].className).toBe("seg active");
    expect(container.querySelector(".seg-filter")).not.toBeNull();
    await act(async () => {
      root.render(
        <AppProvider>
          <FilesPanel key={projects[0].id} project={archivedProject(projects[0])} />
        </AppProvider>,
      );
    });
    await act(async () => {
      await Promise.resolve();
    });
    expect(segs()).toHaveLength(1);
    expect(segs()[0].className).toBe("seg active");
    expect(container.querySelector(".seg-filter")).toBeNull();
    expect(container.querySelector(".task-board")).toBeNull();
  });
});

describe("FilesPanel breadcrumbs home crumb (Q.md item 9)", () => {
  const homeCrumb = () => container.querySelector(".crumbs .crumb-home")!;
  const crumbLabels = () =>
    Array.from(container.querySelectorAll(".crumbs .crumb")).map((c) => c.textContent ?? "");

  it("list view: home is present at root as a span and crumbs appear without a jump", async () => {
    await mount(projects[0]);
    expect(container.querySelector(".crumbs")).not.toBeNull();
    expect(homeCrumb().tagName).toBe("SPAN");
    expect(crumbLabels()).toEqual([""]);
  });

  it("list view: selecting a file makes home clickable and clicking it clears the selection", async () => {
    await mount(projects[0]);
    const row = Array.from(container.querySelectorAll(".tree-row")).find(
      (r) => r.querySelector(".label")?.textContent === "a.txt",
    )!;
    await click(row);
    expect(container.querySelector(".file-preview")).not.toBeNull();
    expect(crumbLabels()).toEqual(["", "a.txt"]);
    expect(homeCrumb().tagName).toBe("BUTTON");
    await click(homeCrumb());
    expect(container.querySelector(".file-preview")).toBeNull();
    expect(homeCrumb().tagName).toBe("SPAN");
  });

  it("list view: selecting a folder shows it as the current non-link crumb", async () => {
    await mount(projects[0]);
    const row = Array.from(container.querySelectorAll(".tree-row")).find(
      (r) => r.querySelector(".label")?.textContent === "docs",
    )!;
    await click(row);
    expect(crumbLabels()).toEqual(["", "docs"]);
    const current = container.querySelectorAll(".crumbs .crumb")[1];
    expect(current.tagName).toBe("SPAN");
    expect(container.querySelectorAll(".crumbs button.crumb")).toHaveLength(1);
  });

  it("grid view: navigating into a folder keeps clickable crumbs and home returns to root", async () => {
    await mount(projects[1]);
    const tile = Array.from(container.querySelectorAll(".tile")).find(
      (t) => t.querySelector(".label")?.textContent === "docs",
    )!;
    await act(() =>
      tile.dispatchEvent(new MouseEvent("dblclick", { bubbles: true })),
    );
    expect(crumbLabels()).toEqual(["", "docs"]);
    const current = container.querySelectorAll(".crumbs .crumb")[1];
    expect(current.tagName).toBe("SPAN");
    await click(homeCrumb());
    expect(crumbLabels()).toEqual([""]);
    expect(homeCrumb().tagName).toBe("SPAN");
  });
});

describe("FilesPanel file drag (list mode)", () => {
  const rowByLabel = (name: string) =>
    Array.from(container.querySelectorAll(".tree-row")).find(
      (r) => r.querySelector(".label")?.textContent === name,
    )!;

  it("clicking a file row in list mode still opens the preview", async () => {
    await mount(projects[0]);
    await click(rowByLabel("a.txt"));
    expect(container.querySelector(".file-preview")).not.toBeNull();
  });

  it("tile mode does not start a pointer drag", async () => {
    await mount(projects[1]);
    const tile = container.querySelector(".tile")!;
    expect(tile).not.toBeNull();
    act(() => {
      tile.dispatchEvent(
        new MouseEvent("pointerdown", { bubbles: true, cancelable: true, button: 0, clientX: 10, clientY: 10 }),
      );
      window.dispatchEvent(
        new MouseEvent("pointermove", { bubbles: true, cancelable: true, button: 0, clientX: 10, clientY: 80 }),
      );
      window.dispatchEvent(
        new MouseEvent("pointermove", { bubbles: true, cancelable: true, button: 0, clientX: 10, clientY: 160 }),
      );
      window.dispatchEvent(
        new MouseEvent("pointerup", { bubbles: true, cancelable: true, button: 0, clientX: 10, clientY: 160 }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });
    expect(vi.mocked(ipc.fsMove)).not.toHaveBeenCalled();
    expect(document.querySelector(".drag-ghost")).toBeNull();
  });
});

describe("FilesPanel active-project root nav (Q.md item 7)", () => {
  async function mountWithTrigger(project: ProjectDto) {
    await act(async () => {
      root.render(
        <AppProvider>
          <FilesPanel key={project.id} project={project} />
          <RootNavTrigger projectId={project.id} />
        </AppProvider>,
      );
    });
    await act(async () => {
      await Promise.resolve();
    });
  }

  const trigger = () => container.querySelector<HTMLButtonElement>(".test-root-nav")!;
  const homeCrumb = () => container.querySelector(".crumbs .crumb-home")!;
  const rowByLabel = (name: string) =>
    Array.from(container.querySelectorAll(".tree-row")).find(
      (r) => r.querySelector(".label")?.textContent === name,
    )!;

  it("a root-nav request after opening a file closes the preview and returns to root", async () => {
    await mountWithTrigger(projects[0]);
    await click(rowByLabel("a.txt"));
    expect(container.querySelector(".file-preview")).not.toBeNull();
    await click(trigger());
    expect(container.querySelector(".file-preview")).toBeNull();
    expect(homeCrumb().tagName).toBe("SPAN");
  });

  it("a root-nav request while already at root is a no-op with no errors", async () => {
    await mountWithTrigger(projects[0]);
    await click(trigger());
    expect(container.querySelector(".file-preview")).toBeNull();
    expect(homeCrumb().tagName).toBe("SPAN");
  });

  it("a root-nav request for another project leaves this panel untouched", async () => {
    await mountWithTrigger(projects[0]);
    await click(rowByLabel("a.txt"));
    expect(container.querySelector(".file-preview")).not.toBeNull();
    await act(async () => {
      root.render(
        <AppProvider>
          <FilesPanel key={projects[0].id} project={projects[0]} />
          <RootNavTrigger projectId={999} />
        </AppProvider>,
      );
    });
    await act(async () => {
      await Promise.resolve();
    });
    await click(container.querySelector<HTMLButtonElement>(".test-root-nav")!);
    expect(container.querySelector(".file-preview")).not.toBeNull();
  });
});

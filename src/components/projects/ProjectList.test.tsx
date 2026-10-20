import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectDto } from "../../api/types";

const projects = vi.hoisted((): ProjectDto[] => []);

const listState = vi.hoisted(() => ({ activeProjectId: null as number | null }));

const actions = vi.hoisted(() => ({
  createProject: vi.fn(async () => {}),
  moveProject: vi.fn(async () => {}),
  openProject: vi.fn(async () => {}),
  renameProject: vi.fn(async () => {}),
  archiveProject: vi.fn(async () => {}),
  clearNewProject: vi.fn(),
}));

vi.mock("../../state/AppStore", () => ({
  useApp: () => ({
    state: {
      projects,
      activeProjectId: listState.activeProjectId,
      newProjectRequested: false,
    },
    actions,
  }),
}));

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import ProjectList from "./ProjectList";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  projects.length = 0;
  listState.activeProjectId = null;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.clearAllMocks();
});

async function mount() {
  await act(async () => {
    root.render(<ProjectList />);
  });
}

describe("ProjectList new-project button (Q-21)", () => {
  it("renders a labeled circular-plus create control with the Russian text and title", async () => {
    await mount();
    const btn = container.querySelector<HTMLButtonElement>(".add-project-btn");
    expect(btn).not.toBeNull();
    expect(btn!.getAttribute("title")).toBe("Новый проект");
    expect(btn!.textContent).toContain("Новый проект");
    expect(btn!.querySelector(".add-project-plus svg")).not.toBeNull();
  });

  it("clicking the create control opens the inline name input (handler unchanged)", async () => {
    await mount();
    expect(container.querySelector(".add-input")).toBeNull();
    const btn = container.querySelector<HTMLButtonElement>(".add-project-btn")!;
    await act(async () => {
      btn.click();
    });
    expect(container.querySelector(".add-input")).not.toBeNull();
  });
});

const makeProject = (id: number, name: string, archived: boolean): ProjectDto => ({
  id,
  name,
  path: `/p/${name}`,
  createdAt: "2026-01-01 09:00:00",
  openedAt: null,
  position: id,
  fileCount: 1,
  taskCount: 0,
  overdueCount: 0,
  hasActiveTasks: false,
  taskColumns: ["Новые", "В работе", "Готово"],
  filesView: "list",
  archived,
  archivedAt: archived ? "2026-01-01 10:00:00" : null,
});

describe("ProjectList current/archive tabs (Q.md item 5)", () => {
  const segs = () => Array.from(container.querySelectorAll(".sidebar-seg .seg"));
  const rowNames = () =>
    Array.from(container.querySelectorAll(".project-item .name")).map((el) => el.textContent);
  const click = (el: Element) =>
    act(async () => {
      el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

  it("renders «Текущие» and «Архивные» tabs with current active by default", async () => {
    await mount();
    expect(segs()).toHaveLength(2);
    expect(segs()[0].textContent).toBe("Текущие");
    expect(segs()[0].className).toBe("seg active");
    expect(segs()[1].textContent).toBe("Архивные");
    expect(segs()[1].className).toBe("seg");
  });

  it("the current tab lists only non-archived projects", async () => {
    projects.push(makeProject(1, "Alpha", false), makeProject(2, "Beta", true));
    await mount();
    expect(rowNames()).toEqual(["Alpha"]);
  });

  it("the archive tab lists only archived projects", async () => {
    projects.push(makeProject(1, "Alpha", false), makeProject(2, "Beta", true));
    await mount();
    await click(segs()[1]);
    expect(segs()[1].className).toBe("seg active");
    expect(rowNames()).toEqual(["Beta"]);
  });

  it("the archive action on a current row archives it via the store action", async () => {
    projects.push(makeProject(1, "Alpha", false));
    await mount();
    const btn = container.querySelector<HTMLButtonElement>(".project-item .archive-toggle-btn")!;
    await click(btn);
    expect(actions.archiveProject).toHaveBeenCalledWith(1, true);
  });

  it("the archive action on an archived row returns it via the store action", async () => {
    projects.push(makeProject(2, "Beta", true));
    await mount();
    await click(segs()[1]);
    const btn = container.querySelector<HTMLButtonElement>(".project-item .archive-toggle-btn")!;
    await click(btn);
    expect(actions.archiveProject).toHaveBeenCalledWith(2, false);
  });

  it("archiving the active project keeps the «Текущие» tab active", async () => {
    listState.activeProjectId = 1;
    projects.push(makeProject(1, "Alpha", false), makeProject(2, "Beta", false));
    await mount();
    const btn = container.querySelector<HTMLButtonElement>(".project-item .archive-toggle-btn")!;
    await click(btn);
    expect(actions.archiveProject).toHaveBeenCalledWith(1, true);
    expect(segs()[0].className).toBe("seg active");
    expect(segs()[1].className).toBe("seg");
  });

  it("archiving a non-active project keeps the «Текущие» tab active", async () => {
    listState.activeProjectId = 1;
    projects.push(makeProject(1, "Alpha", false), makeProject(2, "Beta", false));
    await mount();
    const btns = container.querySelectorAll<HTMLButtonElement>(".project-item .archive-toggle-btn");
    await click(btns[1]);
    expect(actions.archiveProject).toHaveBeenCalledWith(2, true);
    expect(segs()[0].className).toBe("seg active");
  });
});

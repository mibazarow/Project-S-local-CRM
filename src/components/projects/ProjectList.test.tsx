import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectDto } from "../../api/types";

const projects = vi.hoisted((): ProjectDto[] => []);

const listState = vi.hoisted(() => ({ activeProjectId: null as number | null }));

const actions = vi.hoisted(() => ({
  createProject: vi.fn(async () => {}),
  moveProject: vi.fn(async () => {}),
  openProject: vi.fn(async () => {}),
  requestProjectRoot: vi.fn(),
  renameProject: vi.fn(async () => {}),
  archiveProject: vi.fn(async () => {}),
  deleteProject: vi.fn(async () => true),
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
import { registerConfirmHost, type ConfirmRequest } from "../../shared/confirm";
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
  registerConfirmHost(null);
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
  builtin: false,
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

describe("ProjectList builtin project (Q.md item 2)", () => {
  const builtinRow = () =>
    container.querySelector<HTMLElement>(".project-item.builtin")!;

  it("renders the builtin row first with no archive/delete actions", async () => {
    projects.push({ ...makeProject(1, "Мои задачи", false), builtin: true });
    projects.push(makeProject(2, "Alpha", false));
    await mount();
    const rows = Array.from(container.querySelectorAll<HTMLElement>(".project-item"));
    expect(rows[0].classList.contains("builtin")).toBe(true);
    expect(builtinRow().querySelector(".archive-toggle-btn")).toBeNull();
    expect(builtinRow().querySelector(".delete-btn")).toBeNull();
  });

  it("right-clicking it opens no delete context menu", async () => {
    projects.push({ ...makeProject(1, "Мои задачи", false), builtin: true });
    await mount();
    await act(async () => {
      builtinRow().dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, cancelable: true }),
      );
    });
    expect(document.querySelector(".context-menu")).toBeNull();
  });
});

describe("ProjectList active-project name click (Q.md item 7)", () => {
  const click = (el: Element) =>
    act(async () => {
      el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
  const row = (name: string) =>
    Array.from(container.querySelectorAll<HTMLElement>(".project-item")).find(
      (el) => el.querySelector(".name")?.textContent === name,
    )!;

  it("clicking the active project's row requests a root navigation, not a re-open", async () => {
    listState.activeProjectId = 1;
    projects.push(makeProject(1, "Alpha", false));
    await mount();
    await click(row("Alpha"));
    expect(actions.requestProjectRoot).toHaveBeenCalledWith(1);
    expect(actions.openProject).not.toHaveBeenCalled();
  });

  it("clicking an inactive project's row opens it at its root", async () => {
    listState.activeProjectId = 2;
    projects.push(makeProject(1, "Alpha", false), makeProject(2, "Beta", false));
    await mount();
    await click(row("Alpha"));
    expect(actions.openProject).toHaveBeenCalledWith(1);
    expect(actions.requestProjectRoot).not.toHaveBeenCalled();
  });

  it("the archive action on the active row still works and does not trigger root nav", async () => {
    listState.activeProjectId = 1;
    projects.push(makeProject(1, "Alpha", false));
    await mount();
    const btn = container.querySelector<HTMLButtonElement>(".project-item .archive-toggle-btn")!;
    await click(btn);
    expect(actions.archiveProject).toHaveBeenCalledWith(1, true);
    expect(actions.requestProjectRoot).not.toHaveBeenCalled();
    expect(actions.openProject).not.toHaveBeenCalled();
  });
});

describe("ProjectList permanent delete (Q.md item 1)", () => {
  const click = (el: Element) =>
    act(async () => {
      el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
  const row = (name: string) =>
    Array.from(container.querySelectorAll<HTMLElement>(".project-item")).find(
      (el) => el.querySelector(".name")?.textContent === name,
    )!;

  // Fake confirm host: records the request and answers with a fixed verdict.
  function answer(ok: boolean): () => ConfirmRequest | null {
    let last: ConfirmRequest | null = null;
    registerConfirmHost((req) => {
      last = req;
      req.resolve(ok);
    });
    return () => last;
  }

  it("the row trash button asks for an irreversible confirmation, then deletes", async () => {
    const lastRequest = answer(true);
    projects.push(makeProject(1, "Alpha", false));
    await mount();
    await click(container.querySelector<HTMLButtonElement>(".project-item .delete-btn")!);
    const req = lastRequest();
    expect(req).not.toBeNull();
    expect(req!.opts.title).toBe("Удалить проект «Alpha»?");
    expect(req!.opts.message).toContain("безвозвратно");
    expect(req!.opts.danger).toBe(true);
    expect(req!.opts.confirmLabel).toBe("Удалить навсегда");
    expect(actions.deleteProject).toHaveBeenCalledWith(1);
  });

  it("cancelling the confirmation deletes nothing", async () => {
    answer(false);
    projects.push(makeProject(1, "Alpha", false));
    await mount();
    await click(container.querySelector<HTMLButtonElement>(".project-item .delete-btn")!);
    expect(actions.deleteProject).not.toHaveBeenCalled();
  });

  it("right-click opens a context menu whose «Удалить проект…» item runs the same flow", async () => {
    answer(true);
    projects.push(makeProject(1, "Alpha", false));
    await mount();
    await act(async () => {
      row("Alpha").dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 40, clientY: 60 }),
      );
    });
    const items = Array.from(document.querySelectorAll<HTMLButtonElement>(".context-menu-item"));
    expect(items.map((el) => el.textContent)).toContain("Удалить проект…");
    await click(items.find((el) => el.textContent === "Удалить проект…")!);
    expect(actions.deleteProject).toHaveBeenCalledWith(1);
    expect(document.querySelector(".context-menu")).toBeNull();
  });

  it("deleting the active project keeps the «Текущие» tab active", async () => {
    answer(true);
    listState.activeProjectId = 1;
    projects.push(makeProject(1, "Alpha", false), makeProject(2, "Beta", false));
    await mount();
    const btns = container.querySelectorAll<HTMLButtonElement>(".project-item .delete-btn");
    await click(btns[0]);
    expect(actions.deleteProject).toHaveBeenCalledWith(1);
    const segs = Array.from(container.querySelectorAll(".sidebar-seg .seg"));
    expect(segs[0].className).toBe("seg active");
  });
});

import { act, type MouseEvent as ReactMouseEvent } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { ProjectDto } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

import ProjectItem from "./ProjectItem";

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
});

const makeProject = (overrides: Partial<ProjectDto> = {}): ProjectDto => ({
  id: 1,
  name: "example.ru",
  path: "/wd/Projects/example.ru",
  createdAt: "2026-01-01 09:00:00",
  openedAt: null,
  position: 1,
  fileCount: 3,
  taskCount: 5,
  overdueCount: 0,
  hasActiveTasks: false,
  taskColumns: ["Новые", "В работе", "Готово"],
  filesView: "list",
    archived: false,
    archivedAt: null,
    builtin: false,
  ...overrides,
});

async function mount(
  project: ProjectDto,
  onArchive = (_id: number, _archived: boolean) => {},
  onDelete = (_id: number) => {},
  onContextMenu = (_e: ReactMouseEvent<HTMLDivElement>) => {},
) {
  await act(async () => {
    root.render(
      <ProjectItem
        project={project}
        active={false}
        dragging={false}
        onOpen={() => {}}
        onRename={() => {}}
        onArchive={onArchive}
        onDelete={onDelete}
        onContextMenu={onContextMenu}
        onPointerDown={() => {}}
      />,
    );
  });
}

describe("ProjectItem builtin project (Q.md item 2)", () => {
  it("marks the row with the builtin class and hides file/archive/delete actions", async () => {
    await mount(makeProject({ builtin: true, name: "Мои задачи", fileCount: 7, taskCount: 2 }));
    const row = container.querySelector(".project-item")!;
    expect(row.className).toContain("builtin");
    expect(container.querySelector(".proj-actions .archive-toggle-btn")).toBeNull();
    expect(container.querySelector(".proj-actions .delete-btn")).toBeNull();
    expect(container.querySelector<HTMLButtonElement>('.proj-actions button[title="Переименовать проект"]')).not.toBeNull();
    expect(row.querySelector(".meta")!.textContent).toBe("2 задач");
  });

  it("plain projects are not marked as builtin", async () => {
    await mount(makeProject());
    expect(container.querySelector(".project-item")!.className).not.toContain("builtin");
    expect(container.querySelector(".proj-actions .archive-toggle-btn")).not.toBeNull();
    expect(container.querySelector(".proj-actions .delete-btn")).not.toBeNull();
  });
});

describe("ProjectItem overdue line", () => {
  it("renders the overdue count on its own line after the meta line", async () => {
    await mount(makeProject({ overdueCount: 3 }));
    const meta = container.querySelector(".project-item > .meta:not(.meta-overdue)")!;
    const overdue = container.querySelector(".project-item > .meta-overdue")!;
    expect(meta.textContent).not.toContain("просрочено");
    expect(overdue.textContent).toBe("Просрочено 3 задачи");
    expect(overdue.previousElementSibling).toBe(meta);
  });

  it.each([
    [1, "Просрочено 1 задача"],
    [3, "Просрочено 3 задачи"],
    [5, "Просрочено 5 задач"],
    [11, "Просрочено 11 задач"],
    [22, "Просрочено 22 задачи"],
  ])("uses the right Russian plural for %i", async (count, text) => {
    await mount(makeProject({ overdueCount: count }));
    expect(container.querySelector(".meta-overdue")!.textContent).toBe(text);
  });

  it("renders no overdue line when the count is zero", async () => {
    await mount(makeProject({ overdueCount: 0 }));
    expect(container.querySelector(".meta-overdue")).toBeNull();
  });
});

describe("ProjectItem active-task indicator", () => {
  it("shows the Play icon with a Russian tooltip and accessible label when a task is in progress", async () => {
    await mount(makeProject({ hasActiveTasks: true }));
    const icon = container.querySelector(".active-tasks");
    expect(icon).not.toBeNull();
    expect(icon!.getAttribute("aria-label")).toBe("Есть задачи в работе");
    expect(icon!.querySelector("title")?.textContent).toBe("Есть задачи в работе");
  });

  it("renders no Play icon when the project has no progress task", async () => {
    await mount(makeProject({ hasActiveTasks: false }));
    expect(container.querySelector(".active-tasks")).toBeNull();
  });
});

describe("ProjectItem archive action", () => {
  const archiveBtn = () =>
    container.querySelector<HTMLButtonElement>(".proj-actions .archive-toggle-btn")!;

  it("shows an icon plus «В архив» label and tooltip on a current project", async () => {
    await mount(makeProject());
    const btn = archiveBtn();
    expect(btn).not.toBeNull();
    expect(btn.getAttribute("title")).toBe("В архив");
    expect(btn.querySelector("svg")).not.toBeNull();
    expect(btn.querySelector(".btn-label")?.textContent).toBe("В архив");
  });

  it("shows «Вернуть» with the «Вернуть из архива» tooltip on an archived project", async () => {
    await mount(makeProject({ archived: true, archivedAt: "2026-01-01 10:00:00" }));
    const btn = archiveBtn();
    expect(btn.getAttribute("title")).toBe("Вернуть из архива");
    expect(btn.querySelector(".btn-label")?.textContent).toBe("Вернуть");
  });

  it("clicking the action toggles the archive flag through the callback", async () => {
    const calls: [number, boolean][] = [];
    await mount(makeProject(), (id, archived) => calls.push([id, archived]));
    await act(async () => {
      archiveBtn().click();
    });
    expect(calls).toEqual([[1, true]]);
  });

  it("clicking the action on an archived project returns it (archived=false)", async () => {
    const calls: [number, boolean][] = [];
    await mount(makeProject({ archived: true, archivedAt: "2026-01-01 10:00:00" }), (id, archived) =>
      calls.push([id, archived]),
    );
    await act(async () => {
      archiveBtn().click();
    });
    expect(calls).toEqual([[1, false]]);
  });
});

describe("ProjectItem delete action (Q.md item 1)", () => {
  const deleteBtn = () =>
    container.querySelector<HTMLButtonElement>(".proj-actions .delete-btn")!;

  it("renders a danger trash button with the «Удалить проект» tooltip", async () => {
    await mount(makeProject());
    const btn = deleteBtn();
    expect(btn).not.toBeNull();
    expect(btn.getAttribute("title")).toBe("Удалить проект");
    expect(btn.className).toContain("danger");
    expect(btn.querySelector("svg")).not.toBeNull();
  });

  it("clicking it calls onDelete with the project id and does not open the project", async () => {
    const deleted: number[] = [];
    await act(async () => {
      root.render(
        <ProjectItem
          project={makeProject()}
          active={false}
          dragging={false}
          onOpen={() => {
            throw new Error("row click must be suppressed");
          }}
          onRename={() => {}}
          onArchive={() => {}}
          onDelete={(id) => deleted.push(id)}
          onContextMenu={() => {}}
          onPointerDown={() => {}}
        />,
      );
    });
    await act(async () => {
      deleteBtn().click();
    });
    expect(deleted).toEqual([1]);
  });

  it("right-clicking the row forwards the contextmenu event", async () => {
    let seen = 0;
    await mount(makeProject(), () => {}, () => {}, () => {
      seen++;
    });
    await act(async () => {
      container
        .querySelector(".project-item")!
        .dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
    });
    expect(seen).toBe(1);
  });
});

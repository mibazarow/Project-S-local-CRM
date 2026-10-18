import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectDto, TaskDto, TaskPatch } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const tasksList = vi.hoisted(() => vi.fn(async (): Promise<TaskDto[]> => [task]));
const taskCreate = vi.hoisted(() => vi.fn(async (): Promise<TaskDto> => task));
const taskUpdate = vi.hoisted(
  () =>
    vi.fn(async (_id: number, patch: TaskPatch): Promise<TaskDto> => ({
      ...task,
      isPriority: patch.isPriority ?? task.isPriority,
    })),
);

vi.mock("../../api/ipc", () => ({
  listProjects: vi.fn(async () => [project]),
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-test" })),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async () => () => {}),
  onTasksChanged: vi.fn(async () => () => {}),
  onSchedulesChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
  tasksList,
  taskCreate,
  taskMove: vi.fn(async () => task),
  taskUpdate,
  taskDelete: vi.fn(async () => undefined),
  setTaskColumns: vi.fn(async () => undefined),
}));

const task = vi.hoisted((): TaskDto => ({
  id: 1,
  projectId: 1,
  title: "Отчёт",
  status: "new",
  comment: null,
  dueDate: "2099-01-02",
  dueTime: "10:30",
  remindAt: null,
  position: 1,
  createdAt: "2026-01-01 09:00:00",
  closedAt: null,
  overdue: false,
  isPriority: false,
  checklist: [],
  recurrence: "none",
  spentMs: 0,
  startedAt: null,
}));

const project = vi.hoisted(
  (): ProjectDto => ({
    id: 1,
    name: "Alpha",
    path: "/p/alpha",
    createdAt: "2026-01-01T00:00:00Z",
    openedAt: null,
    position: 0,
    fileCount: 0,
    taskCount: 1,
    overdueCount: 0,
    taskColumns: ["Новые", "В работе", "Готово"],
  }),
);

import KanbanBoard from "./KanbanBoard";
import { AppProvider } from "../../state/AppStore";

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

const settle = async (ms = 250) => {
  await act(async () => {
    await new Promise((r) => setTimeout(r, ms));
  });
};

const click = (el: Element) =>
  act(() => el.dispatchEvent(new MouseEvent("click", { bubbles: true })));

const setValue = (el: HTMLInputElement | HTMLTextAreaElement, value: string) => {
  const proto =
    el instanceof HTMLTextAreaElement
      ? window.HTMLTextAreaElement.prototype
      : window.HTMLInputElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(proto, "value")!.set!;
  act(() => {
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
};

describe("KanbanBoard add task form", () => {
  it("sends the entered due time to taskCreate", async () => {
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} />
        </AppProvider>,
      );
    });
    await settle();
    expect(container.querySelectorAll(".task-card")).toHaveLength(1);
    expect(container.querySelector(".task-card .m")!.textContent).toContain("10:30");

    await click(container.querySelector(".task-add-btn")!);
    const form = container.querySelector(".task-add-form")!;
    setValue(form.querySelector("textarea")!, "Новая задача");
    setValue(form.querySelector<HTMLInputElement>('input[type="date"]')!, "2099-01-03");
    setValue(form.querySelector<HTMLInputElement>('input[type="time"]')!, "18:45");
    await click(form.querySelector('button[title="Создать задачу"]')!);
    await settle();

    expect(taskCreate).toHaveBeenCalledWith(
      1,
      "Новая задача",
      "new",
      "2099-01-03",
      "18:45",
      false,
    );
  });

  it("omits due time when only a date is set (backend defaults to 07:00)", async () => {
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} />
        </AppProvider>,
      );
    });
    await settle();
    await click(container.querySelector(".task-add-btn")!);
    const form = container.querySelector(".task-add-form")!;
    setValue(form.querySelector("textarea")!, "Без времени");
    setValue(form.querySelector<HTMLInputElement>('input[type="date"]')!, "2099-01-04");
    await click(form.querySelector('button[title="Создать задачу"]')!);
    await settle();

    expect(taskCreate).toHaveBeenCalledWith(
      1,
      "Без времени",
      "new",
      "2099-01-04",
      null,
      false,
    );
  });
});

describe("KanbanBoard priority toggle", () => {
  it("sets priority via the card details editor and keeps it after save and reload", async () => {
    task.isPriority = false;
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} />
        </AppProvider>,
      );
    });
    await settle();
    let card = container.querySelector(".task-card")!;
    expect(card.className).not.toContain("priority");

    await click(container.querySelector('button[title="Приоритет, повтор, чек-лист"]')!);
    const checkbox = container.querySelector<HTMLInputElement>(
      ".task-details .td-priority input[type='checkbox']",
    )!;
    expect(checkbox.checked).toBe(false);
    await click(checkbox);
    await settle();

    expect(taskUpdate).toHaveBeenCalledWith(1, { isPriority: true });
    // The upserted DTO re-renders the card with priority styling.
    card = container.querySelector(".task-card")!;
    expect(card.className).toContain("priority");

    // Reload (tasks-changed bump): the persisted flag survives the refetch.
    task.isPriority = true;
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={1} highlightTaskId={null} />
        </AppProvider>,
      );
    });
    await settle();
    expect(container.querySelector(".task-card")!.className).toContain("priority");

    // Clear it back through the same editor.
    await click(container.querySelector('button[title="Приоритет, повтор, чек-лист"]')!);
    const off = container.querySelector<HTMLInputElement>(
      ".task-details .td-priority input[type='checkbox']",
    )!;
    expect(off.checked).toBe(true);
    await click(off);
    await settle();
    expect(taskUpdate).toHaveBeenLastCalledWith(1, { isPriority: false });
    expect(container.querySelector(".task-card")!.className).not.toContain("priority");

    task.isPriority = false;
  });
});

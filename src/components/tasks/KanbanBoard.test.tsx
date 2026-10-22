import { readFileSync } from "node:fs";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectDto, TaskDto, TaskPatch } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

// Mutable task list the mocked tasksList returns, so focus/drag tests can
// stage several cards in one column while other tests keep the single-task set.
const taskHolder = vi.hoisted((): { tasks: TaskDto[] } => ({ tasks: [] }));
const tasksList = vi.hoisted(() => vi.fn(async (): Promise<TaskDto[]> => taskHolder.tasks));
const taskCreate = vi.hoisted(() => vi.fn(async (): Promise<TaskDto> => task));
const taskMove = vi.hoisted(
  () =>
    vi.fn(
      async (id: number, status: string, position: number): Promise<TaskDto> => ({
        ...task,
        id,
        status,
        position,
      }),
    ),
);
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
  onScheduleOpen: vi.fn(async () => () => {}),
  onChatOpen: vi.fn(async () => () => {}),
  tasksList,
  taskCreate,
  taskMove,
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
    hasActiveTasks: false,
    taskColumns: ["Новые", "В работе", "Готово"],
    filesView: "list",
    archived: false,
    archivedAt: null,
    builtin: false,
  }),
);

import KanbanBoard from "./KanbanBoard";
import { AppProvider } from "../../state/AppStore";

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  taskHolder.tasks = [task];
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
    setValue(form.querySelector(".task-edit-title")!, "Новая задача");
    setValue(form.querySelector<HTMLInputElement>('input[type="date"]')!, "2099-01-03");
    setValue(form.querySelector<HTMLInputElement>('input[type="time"]')!, "18:45");
    await click(form.querySelector('button[title="Сохранить"]')!);
    await settle();

    expect(taskCreate).toHaveBeenCalledWith(
      1,
      "Новая задача",
      "new",
      "2099-01-03",
      "18:45",
      false,
      [],
    );
  });

  it("keeps the default 07:00 due time when only a date is set", async () => {
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
    setValue(form.querySelector(".task-edit-title")!, "Без времени");
    setValue(form.querySelector<HTMLInputElement>('input[type="date"]')!, "2099-01-04");
    await click(form.querySelector('button[title="Сохранить"]')!);
    await settle();

    expect(taskCreate).toHaveBeenCalledWith(
      1,
      "Без времени",
      "new",
      "2099-01-04",
      "07:00",
      false,
      [],
    );
  });

  it("opens the full editor for a new task with the «Важная» label", async () => {
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
    expect(form.querySelector(".task-edit")).not.toBeNull();
    expect(form.textContent).toContain("Важная");
    expect(form.textContent).not.toContain("Приоритетная");
  });

  it("creates a priority task with its checklist and follow-up comment", async () => {
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
    setValue(form.querySelector(".task-edit-title")!, "Важное дело");
    const comment = form.querySelector(".task-edit-comment")!;
    setValue(comment as HTMLTextAreaElement, "Комментарий");
    const priority = form.querySelector<HTMLInputElement>(
      ".td-priority input[type='checkbox']",
    )!;
    await click(priority);
    const checkInput = form.querySelector<HTMLInputElement>(".td-check-add input")!;
    setValue(checkInput, "Пункт");
    await click(form.querySelector('button[title="Добавить пункт"]')!);
    await click(form.querySelector('button[title="Сохранить"]')!);
    await settle();

    expect(taskCreate).toHaveBeenCalledWith(
      1,
      "Важное дело",
      "new",
      null,
      "07:00",
      true,
      [{ text: "Пункт", done: false }],
    );
    expect(taskUpdate).toHaveBeenCalledWith(1, expect.objectContaining({
      comment: "Комментарий",
    }));
  });
});

describe("KanbanBoard priority toggle", () => {
  it("sets priority via the full card editor and keeps it after save and reload", async () => {
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

    const openEditor = async () => {
      await click(container.querySelector('button[title="Редактировать"]')!);
      return container.querySelector(".task-edit")!;
    };
    const save = async (editor: Element) => {
      await click(editor.querySelector('button[title="Сохранить"]')!);
      await settle();
    };

    let editor = await openEditor();
    const checkbox = editor.querySelector<HTMLInputElement>(
      ".td-priority input[type='checkbox']",
    )!;
    expect(checkbox.checked).toBe(false);
    await click(checkbox);
    await save(editor);

    expect(taskUpdate).toHaveBeenCalledWith(1, expect.objectContaining({ isPriority: true }));
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
    editor = await openEditor();
    const off = editor.querySelector<HTMLInputElement>(
      ".td-priority input[type='checkbox']",
    )!;
    expect(off.checked).toBe(true);
    await click(off);
    await save(editor);
    expect(taskUpdate).toHaveBeenLastCalledWith(
      1,
      expect.objectContaining({ isPriority: false }),
    );
    expect(container.querySelector(".task-card")!.className).not.toContain("priority");

    task.isPriority = false;
  });
});

const makeTask = (o: Partial<TaskDto>): TaskDto => ({
  id: 1,
  projectId: 1,
  title: "T",
  status: "new",
  comment: null,
  dueDate: null,
  dueTime: "07:00",
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
  ...o,
});

describe("KanbanBoard overdue filter", () => {
  const cardIds = () =>
    Array.from(container.querySelectorAll<HTMLElement>(".task-card")).map((el) => el.dataset.id);

  it("keeps every task when overdueOnly is omitted", async () => {
    taskHolder.tasks = [
      makeTask({ id: 1, position: 1 }),
      makeTask({ id: 2, position: 2, overdue: true }),
    ];
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} />
        </AppProvider>,
      );
    });
    await settle();
    expect(cardIds()).toEqual(["1", "2"]);
  });

  it("shows only overdue tasks when overdueOnly is set", async () => {
    taskHolder.tasks = [
      makeTask({ id: 1, position: 1 }),
      makeTask({ id: 2, position: 2, overdue: true }),
    ];
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} overdueOnly />
        </AppProvider>,
      );
    });
    await settle();
    expect(cardIds()).toEqual(["2"]);
  });
});

describe("KanbanBoard focus from Today", () => {
  let scrollSpy: ReturnType<typeof vi.fn>;
  let origRect: (() => DOMRect) | undefined;

  beforeEach(() => {
    scrollSpy = vi.fn();
    (Element.prototype as unknown as { scrollIntoView: unknown }).scrollIntoView = scrollSpy;
  });

  afterEach(() => {
    if (origRect) {
      HTMLElement.prototype.getBoundingClientRect = origRect;
      origRect = undefined;
    }
  });

  const columnCardIds = (status: string) =>
    Array.from(
      container.querySelectorAll(`.task-col[data-status="${status}"] .task-card[data-id]`),
    ).map((el) => Number((el as HTMLElement).dataset.id));

  it("raises the focused task to the top of its column, pulses and scrolls", async () => {
    taskHolder.tasks = [
      makeTask({ id: 1, position: 1 }),
      makeTask({ id: 2, position: 2 }),
      makeTask({ id: 3, position: 3 }),
    ];
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={3} />
        </AppProvider>,
      );
    });
    await settle();
    expect(columnCardIds("new")).toEqual([3, 1, 2]);
    const focused = container.querySelector('.task-card[data-id="3"]')!;
    expect(focused.className).toContain("highlight");
    expect(scrollSpy).toHaveBeenCalled();
  });

  it("restores the original order once focus clears", async () => {
    taskHolder.tasks = [
      makeTask({ id: 1, position: 1 }),
      makeTask({ id: 2, position: 2 }),
      makeTask({ id: 3, position: 3 }),
    ];
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={3} />
        </AppProvider>,
      );
    });
    await settle();
    expect(columnCardIds("new")).toEqual([3, 1, 2]);

    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} />
        </AppProvider>,
      );
    });
    await settle();
    expect(columnCardIds("new")).toEqual([1, 2, 3]);
    expect(container.querySelector(".task-card.highlight")).toBeNull();
  });

  it("the pulse animation runs exactly 3 times and is disabled under reduced motion", () => {
    const css = readFileSync("src/styles/app.css", "utf8");
    const rule = css.match(/\.task-card\.highlight \{[^}]*\}/);
    expect(rule, "the highlight rule must exist").not.toBeNull();
    expect(rule![0]).toContain("animation-iteration-count: 3");
    const reduced = css.match(/@media \(prefers-reduced-motion: reduce\) \{[\s\S]*?\n\}/);
    expect(reduced, "the reduced-motion block must exist").not.toBeNull();
    expect(reduced![0]).toContain(".task-card.highlight { animation: none");
  });

  it("drag/drop after focus still computes the new position from real neighbors", async () => {
    taskHolder.tasks = [
      makeTask({ id: 1, position: 1 }),
      makeTask({ id: 2, position: 2 }),
      makeTask({ id: 3, position: 3 }),
    ];
    origRect = HTMLElement.prototype.getBoundingClientRect;
    const rect = (left: number, top: number, width: number, height: number) =>
      ({
        left,
        top,
        right: left + width,
        bottom: top + height,
        width,
        height,
        x: left,
        y: top,
        toJSON() {},
      }) as DOMRect;
    HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
      const status = this.dataset.status;
      if (status) {
        const left = status === "new" ? 0 : status === "progress" ? 300 : 600;
        return rect(left, 0, 300, 1000);
      }
      const id = this.dataset.id;
      if (id) return rect(0, (Number(id) - 1) * 100, 300, 100);
      return rect(0, 0, 0, 0);
    };

    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={3} />
        </AppProvider>,
      );
    });
    await settle();
    // Focused task 3 is raised to the top for display before the drag starts.
    expect(columnCardIds("new")).toEqual([3, 1, 2]);

    const cardA = container.querySelector('.task-card[data-id="1"]')!;
    const fire = (target: EventTarget, type: string, x: number, y: number) =>
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
    fire(cardA, "pointerdown", 50, 50);
    fire(window, "pointermove", 50, 120);
    fire(window, "pointermove", 50, 250);
    fire(window, "pointerup", 50, 250);
    await settle();

    // Real order [1,2,3]: dropping card 1 at the end lands after card 3 (pos 3) → 4.
    expect(taskMove).toHaveBeenCalledWith(1, "new", 4);
  });
});

describe("KanbanBoard view switch (list view over the same tasks)", () => {
  it("keeps the kanban board DOM when view is kanban", async () => {
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} view="kanban" />
        </AppProvider>,
      );
    });
    await settle();
    expect(container.querySelector(".task-board")).not.toBeNull();
    expect(container.querySelectorAll(".task-col")).toHaveLength(3);
    expect(container.querySelectorAll(".task-card")).toHaveLength(1);
    expect(container.querySelector(".task-list-view")).toBeNull();
  });

  it("renders the list view instead of the board when view is list", async () => {
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} view="list" />
        </AppProvider>,
      );
    });
    await settle();
    expect(container.querySelector(".task-board")).toBeNull();
    expect(container.querySelector(".task-col")).toBeNull();
    expect(container.querySelector(".task-list-view")).not.toBeNull();
    const row = container.querySelector('.task-list-row[data-id="1"]')!;
    expect(row.textContent).toContain("Отчёт");
    expect(row.textContent).toContain("Новые");
  });

  it("applies the overdue filter in the list view too", async () => {
    taskHolder.tasks = [
      makeTask({ id: 1, position: 1 }),
      makeTask({ id: 2, position: 2, overdue: true }),
    ];
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard
            project={project}
            tasksVersion={0}
            highlightTaskId={null}
            overdueOnly
            view="list"
          />
        </AppProvider>,
      );
    });
    await settle();
    const ids = Array.from(container.querySelectorAll<HTMLElement>(".task-list-row")).map(
      (el) => el.dataset.id,
    );
    expect(ids).toEqual(["2"]);
  });
});

describe("KanbanBoard list view actions", () => {
  const mountList = async () => {
    await act(async () => {
      root.render(
        <AppProvider>
          <KanbanBoard project={project} tasksVersion={0} highlightTaskId={null} view="list" />
        </AppProvider>,
      );
    });
    await settle();
  };

  const listButton = (id: number, title: string) =>
    Array.from(container.querySelectorAll(`.task-list-row[data-id="${id}"] button`)).find(
      (b) => (b as HTMLButtonElement).title === title,
    )!;

  it("start moves a new task to progress through taskMove", async () => {
    taskHolder.tasks = [makeTask({ id: 1, status: "new", position: 1 })];
    await mountList();
    await click(listButton(1, "Начать"));
    await settle();
    expect(taskMove).toHaveBeenCalledWith(1, "progress", 1);
    expect(container.querySelector('.task-list-row[data-id="1"] .task-list-dot.progress')).not.toBeNull();
  });

  it("rolls back the optimistic move and shows an error when taskMove fails", async () => {
    taskHolder.tasks = [makeTask({ id: 1, status: "new", position: 1 })];
    taskMove.mockRejectedValueOnce(new Error("boom"));
    await mountList();
    await click(listButton(1, "Начать"));
    await settle();
    expect(container.querySelector(".tree-error")).not.toBeNull();
    expect(container.querySelector('.task-list-row[data-id="1"] .task-list-dot.progress')).toBeNull();
  });

  it("creates a task in the selected status and shows it immediately", async () => {
    taskHolder.tasks = [];
    taskCreate.mockResolvedValueOnce({ ...task, id: 99, status: "progress", title: "Новое" });
    await mountList();
    const select = container.querySelector(".task-list-new-status") as HTMLSelectElement;
    const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, "value")!.set!;
    act(() => {
      setter.call(select, "progress");
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await click(container.querySelector(".task-list-add-btn")!);
    const form = container.querySelector(".task-add-form")!;
    setValue(form.querySelector(".task-edit-title")!, "Новое");
    await click(form.querySelector('button[title="Сохранить"]')!);
    await settle();
    expect(taskCreate).toHaveBeenCalledWith(1, "Новое", "progress", null, "07:00", false, []);
    expect(container.querySelector('.task-list-row[data-id="99"]')).not.toBeNull();
  });
});

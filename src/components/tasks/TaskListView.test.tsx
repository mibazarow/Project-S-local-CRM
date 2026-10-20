import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { TaskDto } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

import TaskListView from "./TaskListView";

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

const mount = async (
  tasks: TaskDto[],
  columns = ["Новые", "В работе", "Готово"],
  overdueOnly = false,
) => {
  await act(async () => {
    root.render(<TaskListView tasks={tasks} columns={columns} overdueOnly={overdueOnly} />);
  });
};

const click = (el: Element) =>
  act(() => el.dispatchEvent(new MouseEvent("click", { bubbles: true })));

const groupHeads = () =>
  Array.from(container.querySelectorAll<HTMLElement>(".task-list-group-head"));

const groupLabel = (head: Element) =>
  head.querySelector(".task-list-group-label")?.textContent ?? "";

const rowsOf = (head: Element) => {
  const section = head.closest("section")!;
  return Array.from(section.querySelectorAll<HTMLElement>(".task-list-row"));
};

const allRowIds = () =>
  Array.from(container.querySelectorAll<HTMLElement>(".task-list-row")).map(
    (el) => Number(el.dataset.id),
  );

describe("TaskListView grouping", () => {
  it("splits tasks into Сейчас/Просрочено/Новые/В работе/Готово without duplicates", async () => {
    await mount([
      makeTask({ id: 1, status: "progress", startedAt: Date.now() - 5000, title: "Active" }),
      makeTask({ id: 2, status: "done", overdue: true, title: "OverdueDone" }),
      makeTask({ id: 3, status: "new", title: "Fresh" }),
      makeTask({ id: 4, status: "progress", startedAt: null, title: "Paused" }),
      makeTask({ id: 5, status: "done", title: "Finished" }),
    ]);
    expect(container.querySelector(".task-list-now")).not.toBeNull();
    expect(container.querySelector('.task-list-now .task-list-row[data-id="1"]')).not.toBeNull();
    const labels = groupHeads().map(groupLabel);
    expect(labels).toEqual(["Просрочено", "Новые", "В работе", "Готово"]);
    const doneHead = groupHeads().find((h) => groupLabel(h) === "Готово")!;
    expect(doneHead.className).toContain("collapsed");
    await click(doneHead);
    expect(allRowIds().sort((a, b) => a - b)).toEqual([1, 2, 3, 4, 5]);
    expect(new Set(allRowIds()).size).toBe(allRowIds().length);
  });

  it("hides the «Сейчас» block when no task has an active timer", async () => {
    await mount([makeTask({ id: 1, status: "progress", startedAt: null })]);
    expect(container.querySelector(".task-list-now")).toBeNull();
  });

  it("uses the project column labels for groups and row status pills", async () => {
    await mount([makeTask({ id: 1, status: "new", title: "Backlog item" })], [
      "Backlog",
      "Doing",
      "Shipped",
    ]);
    expect(groupHeads().map(groupLabel)).toEqual(["Backlog"]);
    expect(container.querySelector(".task-list-status")?.textContent).toBe("Backlog");
  });

  it("sorts by priority, then nearest deadline, then position, then id", async () => {
    await mount([
      makeTask({ id: 1, position: 1, title: "no due" }),
      makeTask({ id: 2, position: 2, dueDate: "2099-01-05", title: "far" }),
      makeTask({ id: 3, position: 3, dueDate: "2099-01-03", title: "near" }),
      makeTask({ id: 4, position: 4, isPriority: true, title: "important" }),
    ]);
    expect(allRowIds()).toEqual([4, 3, 2, 1]);
  });
});

describe("TaskListView active row timer", () => {
  it("ticks the session and total once per second for the active row only", async () => {
    vi.useFakeTimers();
    try {
      vi.setSystemTime(new Date("2026-10-20T12:00:00"));
      await mount([
        makeTask({
          id: 1,
          status: "progress",
          startedAt: Date.now() - 65_000,
          spentMs: 3_600_000,
          title: "Active",
        }),
        makeTask({ id: 2, status: "new", spentMs: 60_000, title: "Idle" }),
      ]);
      const active = container.querySelector('.task-list-row[data-id="1"]')!;
      expect(active.className).toContain("active");
      expect(active.querySelector(".task-list-spent")!.textContent).toBe("00:01:05 · 01:01:05");
      const idle = container.querySelector('.task-list-row[data-id="2"]')!;
      expect(idle.querySelector(".task-list-spent")!.textContent).toBe("00:01:00");

      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(
        container.querySelector('.task-list-row[data-id="1"] .task-list-spent')!.textContent,
      ).toBe("00:01:06 · 01:01:06");
      expect(
        container.querySelector('.task-list-row[data-id="2"] .task-list-spent')!.textContent,
      ).toBe("00:01:00");
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("TaskListView overdue filter", () => {
  it("shows only the overdue group when overdueOnly is set", async () => {
    await mount(
      [
        makeTask({ id: 2, status: "new", overdue: true, title: "Late" }),
        makeTask({ id: 3, status: "done", overdue: true, title: "LateDone" }),
      ],
      ["Новые", "В работе", "Готово"],
      true,
    );
    expect(groupHeads().map(groupLabel)).toEqual(["Просрочено"]);
    expect(allRowIds().sort((a, b) => a - b)).toEqual([2, 3]);
  });

  it("shows «Нет просроченных задач» when the filter matches nothing", async () => {
    await mount([], ["Новые", "В работе", "Готово"], true);
    expect(container.querySelector(".task-list-empty")!.textContent).toBe("Нет просроченных задач");
  });
});

describe("TaskListView done group collapse", () => {
  it("collapses the done group by default and expands it on click", async () => {
    await mount([
      makeTask({ id: 1, status: "new", title: "Open" }),
      makeTask({ id: 2, status: "done", title: "Closed" }),
    ]);
    const doneHead = groupHeads().find((h) => groupLabel(h) === "Готово")!;
    expect(doneHead.className).toContain("collapsed");
    expect(rowsOf(doneHead)).toHaveLength(0);
    await click(doneHead);
    expect(doneHead.className).not.toContain("collapsed");
    expect(rowsOf(doneHead)).toHaveLength(1);
    await click(doneHead);
    expect(rowsOf(doneHead)).toHaveLength(0);
  });

  it("shows «Все задачи выполнены» with only done tasks left", async () => {
    await mount([makeTask({ id: 1, status: "done", title: "Closed" })]);
    expect(container.querySelector(".task-list-empty")!.textContent).toBe("Все задачи выполнены");
    expect(groupHeads().map(groupLabel)).toEqual(["Готово"]);
  });
});

describe("TaskListView empty states and row details", () => {
  it("shows «Пока нет задач» when there are no tasks at all", async () => {
    await mount([]);
    expect(container.querySelector(".task-list-empty")!.textContent).toBe("Пока нет задач");
  });

  it("renders checklist count, mini progress, due date, priority flag and spent time", async () => {
    await mount([
      makeTask({
        id: 1,
        status: "new",
        title: "Каталог",
        dueDate: "2099-01-02",
        dueTime: "10:30",
        isPriority: true,
        spentMs: 5_050_000,
        checklist: [
          { text: "a", done: true },
          { text: "b", done: true },
          { text: "c", done: false },
          { text: "d", done: false },
          { text: "e", done: false },
        ],
      }),
    ]);
    const row = container.querySelector(".task-list-row")!;
    expect(row.className).toContain("priority");
    expect(row.querySelector(".task-list-flag")).not.toBeNull();
    expect(row.querySelector(".task-list-check-count")!.textContent).toBe("2/5");
    expect(row.querySelector(".task-list-check-fill")!.getAttribute("style")).toContain("40%");
    expect(row.querySelector(".task-list-due")!.textContent).toContain("10:30");
    expect(row.querySelector(".task-list-due")!.className).not.toContain("overdue");
    expect(row.querySelector(".task-list-spent")!.textContent).toBe("01:24:10");
  });

  it("marks the due date red for an overdue row", async () => {
    await mount([makeTask({ id: 1, status: "new", overdue: true, dueDate: "2020-01-02" })]);
    const due = container.querySelector(".task-list-due")!;
    expect(due.className).toContain("overdue");
  });
});

const cb = {
  onPatch: vi.fn(),
  onDelete: vi.fn(),
  onDiscuss: vi.fn(),
  onCreate: vi.fn(),
  onMove: vi.fn(),
};

const resetCb = () => Object.values(cb).forEach((f) => f.mockClear());

const mountWith = async (tasks: TaskDto[], columns = ["Новые", "В работе", "Готово"]) => {
  await act(async () => {
    root.render(
      <TaskListView
        tasks={tasks}
        columns={columns}
        overdueOnly={false}
        projectId={1}
        onPatch={cb.onPatch}
        onDelete={cb.onDelete}
        onDiscuss={cb.onDiscuss}
        onCreate={cb.onCreate}
        onMove={cb.onMove}
      />,
    );
  });
};

const rowButton = (id: number, title: string) =>
  Array.from(container.querySelectorAll(`.task-list-row[data-id="${id}"] button`)).find(
    (b) => (b as HTMLButtonElement).title === title,
  )!;

const setSelect = (el: HTMLSelectElement, value: string) => {
  const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, "value")!.set!;
  act(() => {
    setter.call(el, value);
    el.dispatchEvent(new Event("change", { bubbles: true }));
  });
};

const setTextarea = (el: HTMLTextAreaElement, value: string) => {
  const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value")!.set!;
  act(() => {
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
};

describe("TaskListView row actions", () => {
  beforeEach(resetCb);

  it("start moves a new task to progress at the end of the group", async () => {
    await mountWith([makeTask({ id: 1, status: "new", position: 1 })]);
    await click(rowButton(1, "Начать"));
    expect(cb.onMove).toHaveBeenCalledWith(1, "progress", 1);
  });

  it("complete moves a progress task to done", async () => {
    await mountWith([makeTask({ id: 1, status: "progress", startedAt: null, position: 1 })]);
    await click(rowButton(1, "Готово"));
    expect(cb.onMove).toHaveBeenCalledWith(1, "done", 1);
  });

  it("reopen moves a done task back to new", async () => {
    await mountWith([makeTask({ id: 1, status: "done", position: 1 })]);
    const doneHead = groupHeads().find((h) => groupLabel(h) === "Готово")!;
    await click(doneHead);
    await click(rowButton(1, "Вернуть в новые"));
    expect(cb.onMove).toHaveBeenCalledWith(1, "new", 1);
  });

  it("agent and delete reuse the passed callbacks", async () => {
    const task = makeTask({ id: 1, status: "new", position: 1 });
    await mountWith([task]);
    await click(rowButton(1, "Обсудить с агентом"));
    expect(cb.onDiscuss).toHaveBeenCalledWith(task);
    await click(rowButton(1, "Удалить"));
    expect(cb.onDelete).toHaveBeenCalledWith(task);
  });
});

describe("TaskListView inline editing", () => {
  beforeEach(resetCb);

  it("expands the shared editor and saves the patch", async () => {
    await mountWith([makeTask({ id: 1, status: "new", position: 1, title: "До" })]);
    await click(rowButton(1, "Редактировать"));
    const editor = container.querySelector(".task-list-rows .task-edit")!;
    setTextarea(editor.querySelector(".task-edit-title") as HTMLTextAreaElement, "После");
    await click(
      Array.from(editor.querySelectorAll("button")).find(
        (b) => (b as HTMLButtonElement).title === "Сохранить",
      )!,
    );
    expect(cb.onPatch).toHaveBeenCalledWith(1, expect.objectContaining({ title: "После" }));
    expect(container.querySelector(".task-edit")).toBeNull();
  });

  it("keeps only one row expanded at a time", async () => {
    await mountWith([
      makeTask({ id: 1, status: "new", position: 1 }),
      makeTask({ id: 2, status: "new", position: 2 }),
    ]);
    await click(rowButton(1, "Редактировать"));
    await click(rowButton(2, "Редактировать"));
    expect(container.querySelectorAll(".task-list-rows .task-edit")).toHaveLength(1);
  });

  it("saves an inline comment via taskUpdate patch", async () => {
    await mountWith([makeTask({ id: 1, status: "new", position: 1 })]);
    await click(rowButton(1, "Комментарий"));
    const box = container.querySelector<HTMLTextAreaElement>(
      ".task-list-rows textarea[placeholder='Комментарий…']",
    )!;
    box.value = "Заметка";
    act(() => box.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })));
    expect(cb.onPatch).toHaveBeenCalledWith(1, { comment: "Заметка" });
  });
});

describe("TaskListView creation", () => {
  beforeEach(resetCb);

  it("creates with the status chosen in the header select", async () => {
    await mountWith([makeTask({ id: 1, status: "new", position: 1 })]);
    setSelect(container.querySelector(".task-list-new-status") as HTMLSelectElement, "progress");
    await click(container.querySelector(".task-list-add-btn")!);
    const form = container.querySelector(".task-add-form")!;
    setTextarea(form.querySelector(".task-edit-title") as HTMLTextAreaElement, "Новое");
    await click(
      Array.from(form.querySelectorAll("button")).find(
        (b) => (b as HTMLButtonElement).title === "Сохранить",
      )!,
    );
    expect(cb.onCreate).toHaveBeenCalledWith("progress", expect.objectContaining({ title: "Новое" }));
  });
});

describe("TaskListView drag and drop", () => {
  beforeEach(() => {
    resetCb();
  });

  const origRect = HTMLElement.prototype.getBoundingClientRect;

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

  const installRects = (groupTops: Record<string, [number, number]>, rowTops: Record<string, number>) => {
    HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
      const group = this.dataset.group;
      if (group) {
        const t = groupTops[group];
        return t ? rect(0, t[0], 800, t[1]) : rect(0, 0, 0, 0);
      }
      const id = this.dataset.id;
      if (id) return rect(0, rowTops[id], 800, 40);
      return rect(0, 0, 0, 0);
    };
  };

  const fire = (target: EventTarget, type: string, x: number, y: number) =>
    act(() => {
      target.dispatchEvent(
        new MouseEvent(type, { bubbles: true, cancelable: true, button: 0, clientX: x, clientY: y }),
      );
    });

  afterEach(() => {
    HTMLElement.prototype.getBoundingClientRect = origRect;
  });

  it("reordering inside a group moves at the computed position", async () => {
    installRects({ news: [0, 100] }, { "1": 0, "2": 40 });
    await mountWith([
      makeTask({ id: 1, status: "new", position: 1 }),
      makeTask({ id: 2, status: "new", position: 2 }),
    ]);
    const row = container.querySelector('.task-list-row[data-id="1"]')!;
    fire(row, "pointerdown", 10, 20);
    fire(window, "pointermove", 10, 70);
    fire(window, "pointerup", 10, 70);
    expect(cb.onMove).toHaveBeenCalledWith(1, "new", 3);
  });

  it("dragging between groups changes the status", async () => {
    installRects({ news: [0, 100], progress: [200, 100] }, { "1": 0, "2": 200 });
    await mountWith([
      makeTask({ id: 1, status: "new", position: 1 }),
      makeTask({ id: 2, status: "progress", startedAt: null, position: 2 }),
    ]);
    const row = container.querySelector('.task-list-row[data-id="1"]')!;
    fire(row, "pointerdown", 10, 20);
    fire(window, "pointermove", 10, 260);
    fire(window, "pointerup", 10, 260);
    expect(cb.onMove).toHaveBeenCalledWith(1, "progress", 3);
  });
});

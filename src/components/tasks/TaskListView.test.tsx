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

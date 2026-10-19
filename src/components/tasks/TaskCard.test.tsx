import { readFileSync } from "node:fs";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { TaskDto } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

import TaskCard from "./TaskCard";

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
  vi.useRealTimers();
});

const makeTask = (overrides: Partial<TaskDto> = {}): TaskDto => ({
  id: 1,
  projectId: 1,
  title: "Собрать отчёт",
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
  ...overrides,
});

async function renderCard(task: TaskDto, onPatch = vi.fn()) {
  await act(async () => {
    root.render(
      <TaskCard
        task={task}
        dragging={false}
        onPointerDown={() => {}}
        onPatch={onPatch}
        onDelete={() => {}}
      />,
    );
  });
  return onPatch;
}

const mount = renderCard;

const click = (el: Element) =>
  act(() => el.dispatchEvent(new MouseEvent("click", { bubbles: true })));

const setInputValue = (input: HTMLInputElement, value: string) => {
  const setter = Object.getOwnPropertyDescriptor(
    window.HTMLInputElement.prototype,
    "value",
  )!.set!;
  act(() => {
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
};

const findButton = (scope: ParentNode, title: string) =>
  Array.from(scope.querySelectorAll("button")).find(
    (b) => (b as HTMLButtonElement).title === title,
  )!;

const openEditor = async () => {
  await click(findButton(container, "Редактировать"));
  return container.querySelector(".task-edit")!;
};

const clickSave = async (editor: Element) => {
  await click(findButton(editor, "Сохранить"));
};

describe("TaskCard due time", () => {
  it("shows the due time next to the date", async () => {
    await mount(makeTask());
    const meta = container.querySelector(".task-card .m")!;
    expect(meta.textContent).toContain("10:30");
    expect(meta.querySelector(".due-time")!.textContent).toBe("10:30");
  });

  it("marks the card overdue from the backend flag (date+time based)", async () => {
    await mount(makeTask({ overdue: true }));
    const card = container.querySelector(".task-card")!;
    expect(card.className).toContain("overdue");
    expect(container.querySelector(".task-card .m span.red")).not.toBeNull();
  });

  it("the due editor edits the due time and patches it", async () => {
    const onPatch = await mount(makeTask());
    const editor = await openEditor();
    const times = editor.querySelectorAll<HTMLInputElement>('input[type="time"]');
    expect(times).toHaveLength(2);
    expect(times[0].value).toBe("10:30");
    setInputValue(times[0], "18:45");
    await clickSave(editor);
    expect(onPatch).toHaveBeenCalledWith(
      expect.objectContaining({
        dueDate: "2099-01-02",
        dueTime: "18:45",
        remindAt: null,
      }),
    );
  });

  it("an empty due time in the editor falls back to 07:00", async () => {
    const onPatch = await mount(makeTask({ dueTime: "" }));
    const editor = await openEditor();
    const times = editor.querySelectorAll<HTMLInputElement>('input[type="time"]');
    expect(times[0].value).toBe("07:00");
    await clickSave(editor);
    expect(onPatch).toHaveBeenCalledWith(
      expect.objectContaining({
        dueDate: "2099-01-02",
        dueTime: "07:00",
        remindAt: null,
      }),
    );
  });
});

describe("TaskCard priority toggle", () => {
  const openPriority = async () => {
    const editor = await openEditor();
    return editor.querySelector(".td-priority input[type='checkbox']") as HTMLInputElement;
  };

  it("marks the card with the priority class only when the flag is set", async () => {
    await mount(makeTask({ isPriority: true }));
    expect(container.querySelector(".task-card")!.className).toContain("priority");
    await act(async () => {
      root.unmount();
    });
    root = createRoot(container);
    await mount(makeTask({ isPriority: false }));
    expect(container.querySelector(".task-card")!.className).not.toContain("priority");
  });

  it("an overdue card without priority does not get the priority class", async () => {
    await mount(makeTask({ overdue: true, isPriority: false }));
    const card = container.querySelector(".task-card")!;
    expect(card.className).toContain("overdue");
    expect(card.className).not.toContain("priority");
  });

  it("an important overdue card carries both priority and overdue classes", async () => {
    await mount(makeTask({ overdue: true, isPriority: true }));
    const card = container.querySelector(".task-card")!;
    expect(card.className).toContain("priority");
    expect(card.className).toContain("overdue");
  });

  it("the stylesheet keeps overdue pink, gives priority a red border on white, and uses the beige column", () => {
    const css = readFileSync("src/styles/app.css", "utf8");
    const priority = css.match(/\.task-card\.priority \{[^}]*\}/);
    expect(priority, "the priority rule must exist").not.toBeNull();
    expect(priority![0]).toContain("border-color: var(--danger)");
    expect(priority![0]).toContain("background: #fff");
    const overdue = css.match(/\.task-card\.overdue \{[^}]*\}/);
    expect(overdue, "the overdue rule must exist").not.toBeNull();
    expect(overdue![0]).toContain("background: var(--danger-soft)");
    const both = css.match(/\.task-card\.priority\.overdue \{[^}]*\}/);
    expect(both, "the combined priority+overdue rule must exist").not.toBeNull();
    expect(both![0]).toContain("background: var(--danger-soft)");
    const column = css.match(/\.task-col \{[^}]*\}/);
    expect(column, "the kanban column rule must exist").not.toBeNull();
    expect(column![0]).toContain("#fbeeda");
    const foot = css.match(/\.task-card \.task-foot \{[^}]*\}/);
    expect(foot, "the task footer rule must exist").not.toBeNull();
    expect(foot![0]).toContain("margin-top: 10px");
  });

  it("the full editor shows the checkbox reflecting the current flag", async () => {
    await mount(makeTask({ isPriority: true }));
    const checkbox = await openPriority();
    expect(checkbox.checked).toBe(true);
  });

  it("re-syncs the priority checkbox when the task prop changes while the editor is open", async () => {
    await mount(makeTask({ isPriority: false }));
    const checkbox = await openPriority();
    expect(checkbox.checked).toBe(false);
    await renderCard(makeTask({ isPriority: true }));
    expect(
      (container.querySelector(".td-priority input[type='checkbox']") as HTMLInputElement).checked,
    ).toBe(true);
  });

  it("checking the box in the editor saves isPriority: true", async () => {
    const onPatch = await mount(makeTask({ isPriority: false }));
    const checkbox = await openPriority();
    await click(checkbox);
    await clickSave(container.querySelector(".task-edit")!);
    expect(onPatch).toHaveBeenCalledWith(expect.objectContaining({ isPriority: true }));
  });

  it("unchecking the box in the editor saves isPriority: false", async () => {
    const onPatch = await mount(makeTask({ isPriority: true }));
    const checkbox = await openPriority();
    await click(checkbox);
    await clickSave(container.querySelector(".task-edit")!);
    expect(onPatch).toHaveBeenCalledWith(expect.objectContaining({ isPriority: false }));
  });
});

describe("TaskCard checklist", () => {
  const checklist = [
    { text: "Первый пункт", done: false },
    { text: "Второй пункт", done: true },
  ];

  it("renders checklist rows in view mode when the checklist is non-empty", async () => {
    await mount(makeTask({ checklist }));
    const head = container.querySelector(".task-checklist .task-checklist-head")!;
    expect(head.textContent).toContain("Чек-лист");
    expect(head.querySelector(".check-count")!.textContent).toBe("1/2");
    expect(container.querySelectorAll(".task-checklist .td-check-row")).toHaveLength(2);
  });

  it("clicking a checklist checkbox in view mode patches the toggled list immediately", async () => {
    const onPatch = await mount(makeTask({ checklist }));
    const box = container.querySelector<HTMLInputElement>(
      ".task-checklist .td-check-row input[type='checkbox']",
    )!;
    await click(box);
    expect(onPatch).toHaveBeenCalledWith({
      checklist: [
        { text: "Первый пункт", done: true },
        { text: "Второй пункт", done: true },
      ],
    });
  });

  it("the full editor adds a checklist item and Save includes it", async () => {
    const onPatch = await mount(makeTask());
    const editor = await openEditor();
    const input = editor.querySelector<HTMLInputElement>(".td-check-add input")!;
    setInputValue(input, "Новый пункт");
    await click(findButton(editor, "Добавить пункт"));
    await clickSave(editor);
    expect(onPatch).toHaveBeenCalledWith(
      expect.objectContaining({
        checklist: [{ text: "Новый пункт", done: false }],
      }),
    );
  });
});

describe("TaskCard action row", () => {
  it("no longer renders the calendar and old details buttons", async () => {
    await mount(makeTask());
    expect(findButton(container, "Срок и напоминание")).toBeUndefined();
    expect(findButton(container, "Приоритет, повтор, чек-лист")).toBeUndefined();
  });

  it("opens the full editor from the pencil and from a title double-click", async () => {
    await mount(makeTask());
    await click(findButton(container, "Редактировать"));
    expect(container.querySelector(".task-edit")).not.toBeNull();
    await click(findButton(container, "Отменить"));
    expect(container.querySelector(".task-edit")).toBeNull();
    await act(() =>
      container
        .querySelector(".task-card .t")!
        .dispatchEvent(new MouseEvent("dblclick", { bubbles: true })),
    );
    expect(container.querySelector(".task-edit")).not.toBeNull();
  });
});

describe("TaskCard overdue label", () => {
  it("renders a red «Просрочено» caption directly under the title when overdue", async () => {
    await mount(makeTask({ overdue: true }));
    const label = container.querySelector(".task-card .overdue-label");
    expect(label).not.toBeNull();
    expect(label!.textContent).toBe("Просрочено");
    // Placed immediately after the title element, before the due meta row.
    const title = container.querySelector(".task-card .t")!;
    expect(label!.previousElementSibling).toBe(title);
    expect(label!.nextElementSibling).toBe(container.querySelector(".task-card .m"));
  });

  it("does not render the caption when not overdue", async () => {
    await mount(makeTask({ overdue: false }));
    expect(container.querySelector(".task-card .overdue-label")).toBeNull();
  });

  it("the label follows the overdue flag across the 07:00 boundary (06:59 vs 07:01)", async () => {
    // The date+time arithmetic lives in dueMs (format.test.ts); the card just
    // reflects the backend `overdue` flag, so the boundary is covered by it.
    await mount(makeTask({ dueDate: "2026-10-18", dueTime: "06:59", overdue: true }));
    expect(container.querySelector(".task-card .overdue-label")?.textContent).toBe("Просрочено");
    await act(async () => {
      root.unmount();
    });
    root = createRoot(container);
    await mount(makeTask({ dueDate: "2026-10-18", dueTime: "07:01", overdue: false }));
    expect(container.querySelector(".task-card .overdue-label")).toBeNull();
  });

  it("keeps the card DOM/classes stable with and without the label", async () => {
    await mount(makeTask({ overdue: true }));
    const card = container.querySelector(".task-card")!;
    expect(card.className).toContain("task-card");
    expect(card.className).toContain("overdue");
    expect(card.querySelector(".t")).not.toBeNull();
    expect(card.querySelector(".m")).not.toBeNull();
  });
});

describe("TaskCard spent timer footer", () => {
  const footer = () => container.querySelector(".task-card .task-spent");

  it("shows the static total for a stopped task with the footer text", async () => {
    await mount(makeTask({ status: "done", spentMs: 3_661_000 }));
    expect(footer()?.textContent).toBe("Затрачено: 01:01:01");
  });

  it("hides the footer at zero while no work has started", async () => {
    await mount(makeTask({ status: "new", spentMs: 0, startedAt: null }));
    expect(footer()).toBeNull();
  });

  it("places the action row after the spent text inside the card footer", async () => {
    await mount(makeTask({ status: "done", spentMs: 3_661_000 }));
    const foot = container.querySelector(".task-card .task-foot")!;
    expect(foot).not.toBeNull();
    const spent = foot.querySelector(".task-spent")!;
    const actions = foot.querySelector(".task-actions")!;
    expect(spent).not.toBeNull();
    expect(actions).not.toBeNull();
    expect(spent.nextElementSibling).toBe(actions);
  });

  it("shows the footer for a running task even before a second passes", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 9, 18, 12, 0, 0));
    await mount(makeTask({ status: "progress", spentMs: 0, startedAt: Date.now() }));
    expect(footer()?.textContent).toBe("Затрачено: 00:00:00");
  });

  it("ticks once per second by wall clock while in progress", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 9, 18, 12, 0, 5));
    await mount(makeTask({ status: "progress", spentMs: 0, startedAt: Date.now() - 5000 }));
    expect(footer()?.textContent).toBe("Затрачено: 00:00:05");
    await act(async () => {
      vi.advanceTimersByTime(1000);
    });
    expect(footer()?.textContent).toBe("Затрачено: 00:00:06");
  });

  it("adds the live run to the banked time when resuming", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 9, 18, 12, 0, 10));
    await mount(
      makeTask({ status: "progress", spentMs: 3_600_000, startedAt: Date.now() - 10_000 }),
    );
    expect(footer()?.textContent).toBe("Затрачено: 01:00:10");
  });

  it("stays static for stopped tasks across ticks", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 9, 18, 12, 0, 0));
    await mount(makeTask({ status: "new", spentMs: 7_261_000, startedAt: null }));
    const before = footer()?.textContent;
    expect(before).toBe("Затрачено: 02:01:01");
    await act(async () => {
      vi.advanceTimersByTime(3000);
    });
    expect(footer()?.textContent).toBe(before);
  });

  it("completed task titles carry no strikethrough in the stylesheet", () => {
    const css = readFileSync("src/styles/app.css", "utf8");
    const rule = css.match(/\.task-card\.done \.t \{[^}]*\}/);
    expect(rule, "the done title rule must exist").not.toBeNull();
    expect(rule![0]).not.toContain("line-through");
    expect(rule![0]).toContain("var(--text-muted)");
  });
});

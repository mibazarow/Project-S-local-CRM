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
  ...overrides,
});

async function mount(task: TaskDto, onPatch = vi.fn()) {
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
    const dueBtn = Array.from(container.querySelectorAll("button.mini-btn")).find(
      (b) => (b as HTMLButtonElement).title === "Срок и напоминание",
    )!;
    await click(dueBtn);
    const row = container.querySelector(".task-due-row")!;
    const times = row.querySelectorAll<HTMLInputElement>('input[type="time"]');
    expect(times).toHaveLength(2);
    expect(times[0].value).toBe("10:30");
    setInputValue(times[0], "18:45");
    const save = Array.from(row.querySelectorAll("button")).find(
      (b) => (b as HTMLButtonElement).title === "Сохранить дату",
    )!;
    await click(save);
    expect(onPatch).toHaveBeenCalledWith({
      dueDate: "2099-01-02",
      dueTime: "18:45",
      remindAt: null,
    });
  });

  it("an empty due time in the editor falls back to 07:00", async () => {
    const onPatch = await mount(makeTask({ dueTime: "" }));
    const dueBtn = Array.from(container.querySelectorAll("button.mini-btn")).find(
      (b) => (b as HTMLButtonElement).title === "Срок и напоминание",
    )!;
    await click(dueBtn);
    const row = container.querySelector(".task-due-row")!;
    const times = row.querySelectorAll<HTMLInputElement>('input[type="time"]');
    expect(times[0].value).toBe("07:00");
    const save = Array.from(row.querySelectorAll("button")).find(
      (b) => (b as HTMLButtonElement).title === "Сохранить дату",
    )!;
    await click(save);
    expect(onPatch).toHaveBeenCalledWith({
      dueDate: "2099-01-02",
      dueTime: "07:00",
      remindAt: null,
    });
  });
});

describe("TaskCard priority toggle", () => {
  const openDetails = async () => {
    const btn = Array.from(container.querySelectorAll("button.mini-btn")).find(
      (b) => (b as HTMLButtonElement).title === "Приоритет, повтор, чек-лист",
    )!;
    await click(btn);
    return container.querySelector(".task-details .td-priority input[type='checkbox']") as HTMLInputElement;
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

  it("the details editor shows the checkbox reflecting the current flag", async () => {
    await mount(makeTask({ isPriority: true }));
    const checkbox = await openDetails();
    expect(checkbox.checked).toBe(true);
  });

  it("checking the box in the details editor patches isPriority: true", async () => {
    const onPatch = await mount(makeTask({ isPriority: false }));
    const checkbox = await openDetails();
    await click(checkbox);
    expect(onPatch).toHaveBeenCalledWith({ isPriority: true });
  });

  it("unchecking the box in the details editor patches isPriority: false", async () => {
    const onPatch = await mount(makeTask({ isPriority: true }));
    const checkbox = await openDetails();
    await click(checkbox);
    expect(onPatch).toHaveBeenCalledWith({ isPriority: false });
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

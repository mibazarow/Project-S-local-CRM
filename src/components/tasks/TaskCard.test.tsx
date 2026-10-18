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

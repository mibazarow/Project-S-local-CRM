import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { FsEntry, ProjectDto, TaskDto } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

// Deadlines relative to now so the overdue/soon buckets stay correct whenever
// the suite runs (overdue far in the past, soon two days ahead).
const soonDate = vi.hoisted(
  () => new Date(Date.now() + 2 * 24 * 60 * 60 * 1000).toISOString().slice(0, 10),
);

const overdueTask = vi.hoisted((): TaskDto => ({
  id: 101,
  projectId: 1,
  title: "Overdue task",
  status: "new",
  comment: null,
  dueDate: "2000-01-01",
  dueTime: "09:00",
  remindAt: null,
  position: 1,
  createdAt: "2026-01-01 09:00:00",
  closedAt: null,
  overdue: true,
  isPriority: false,
  checklist: [],
  recurrence: "none",
  spentMs: 0,
  startedAt: null,
}));

const soonTask = vi.hoisted((): TaskDto => ({
  id: 102,
  projectId: 1,
  title: "Soon task",
  status: "new",
  comment: null,
  dueDate: soonDate,
  dueTime: "09:00",
  remindAt: null,
  position: 2,
  createdAt: "2026-01-01 09:00:00",
  closedAt: null,
  overdue: false,
  isPriority: false,
  checklist: [],
  recurrence: "none",
  spentMs: 0,
  startedAt: null,
}));

const project = vi.hoisted((): ProjectDto => ({
  id: 1,
  name: "Alpha",
  path: "/p/alpha",
  createdAt: "2026-01-01T00:00:00Z",
  openedAt: null,
  position: 0,
  fileCount: 0,
  taskCount: 2,
  overdueCount: 1,
  hasActiveTasks: false,
  taskColumns: ["Новые", "В работе", "Готово"],
  filesView: "list",
}));

const noteEntry = vi.hoisted((): FsEntry => ({
  name: "Note one",
  relPath: "note-one.md",
  kind: "file",
  ext: "md",
  size: 10,
  modified: Math.floor(Date.now() / 1000),
  isMarkdown: true,
  isText: true,
  isImage: false,
}));

vi.mock("../../api/ipc", () => ({
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-test" })),
  listProjects: vi.fn(async () => [project]),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async () => () => {}),
  onTasksChanged: vi.fn(async () => () => {}),
  onSchedulesChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
  onObjectivesChanged: vi.fn(async () => () => {}),
  onNotesChanged: vi.fn(async () => () => {}),
  tasksList: vi.fn(async (projectId: number) => (projectId === 1 ? [overdueTask, soonTask] : [])),
  scheduleList: vi.fn(async () => []),
  objectiveList: vi.fn(async () => []),
  notesTree: vi.fn(async () => [noteEntry]),
  harnessListSessions: vi.fn(async () => []),
}));

import TodayView from "./TodayView";
import { AppProvider, useAppState } from "../../state/AppStore";

let container: HTMLDivElement;
let root: Root;
let latestState: ReturnType<typeof useAppState> | undefined;

function StateProbe() {
  latestState = useAppState();
  return null;
}

beforeEach(() => {
  latestState = undefined;
  localStorage.clear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

const settle = async (ms = 60) => {
  await act(async () => {
    await new Promise((r) => setTimeout(r, ms));
  });
};

const click = (el: Element) =>
  act(() => el.dispatchEvent(new MouseEvent("click", { bubbles: true })));

const rowByTitle = (title: string) =>
  Array.from(container.querySelectorAll(".today-row")).find(
    (r) => r.querySelector(".today-row-title")?.textContent === title,
  )!;

const mount = async (onTab = vi.fn()) => {
  await act(async () => {
    root.render(
      <AppProvider>
        <TodayView onTab={onTab} />
        <StateProbe />
      </AppProvider>,
    );
  });
  await settle();
  return onTab;
};

describe("TodayView task navigation", () => {
  it("overdue task click opens its project, switches to the projects tab and requests the task", async () => {
    const onTab = await mount();
    expect(rowByTitle("Overdue task")).toBeTruthy();

    await click(rowByTitle("Overdue task"));

    expect(onTab).toHaveBeenCalledWith("projects");
    expect(latestState?.activeProjectId).toBe(1);
    expect(latestState?.pendingTaskId).toBe(101);
  });

  it("soon task click requests that specific task", async () => {
    const onTab = await mount();
    expect(rowByTitle("Soon task")).toBeTruthy();

    await click(rowByTitle("Soon task"));

    expect(onTab).toHaveBeenCalledWith("projects");
    expect(latestState?.pendingTaskId).toBe(102);
  });

  it("a note row opens notes without setting a task intent", async () => {
    const onTab = await mount();
    expect(rowByTitle("Note one")).toBeTruthy();

    await click(rowByTitle("Note one"));

    expect(onTab).toHaveBeenCalledWith("notes");
    expect(latestState?.pendingTaskId).toBeNull();
    expect(latestState?.pendingNotePath).toBe("note-one.md");
  });
});

const DEFAULT_TITLES = [
  "Просрочено",
  "Скоро (7 дней)",
  "Расписания",
  "Цели",
  "Свежие заметки",
  "Последние чаты",
];

const cardTitles = () =>
  Array.from(container.querySelectorAll(".today-card-title")).map((t) => t.textContent);

const readPersistedOrder = () => {
  const raw = localStorage.getItem("today-card-order");
  return raw === null ? null : (JSON.parse(raw) as string[]);
};

describe("TodayView card reorder", () => {
  let origRect: typeof HTMLElement.prototype.getBoundingClientRect;

  beforeEach(() => {
    origRect = HTMLElement.prototype.getBoundingClientRect;
    // Stack the six cards vertically: card i occupies top i*100..i*100+100.
    HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
      const cards = Array.from(container.querySelectorAll<HTMLElement>(".today-card"));
      const i = cards.indexOf(this);
      const top = i >= 0 ? i * 100 : 0;
      return {
        left: 0,
        top,
        right: 300,
        bottom: top + 100,
        width: 300,
        height: 100,
        x: 0,
        y: top,
        toJSON() {},
      } as DOMRect;
    };
  });

  afterEach(() => {
    HTMLElement.prototype.getBoundingClientRect = origRect;
  });

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

  it("renders the six cards in the default order and persists nothing", async () => {
    await mount();
    expect(cardTitles()).toEqual(DEFAULT_TITLES);
    expect(readPersistedOrder()).toBeNull();
    // The removed header summary must not render any counter chips.
    expect(container.querySelector(".today-summary")).toBeNull();
    expect(container.querySelectorAll(".today-chip")).toHaveLength(0);
  });

  it("dragging a card header reorders the DOM and persists the order globally", async () => {
    await mount();
    const card = container.querySelector('.today-card[data-card-id="overdue"]')!;
    const head = card.querySelector(".today-card-head")!;

    fire(head, "pointerdown", 10, 10);
    fire(window, "pointermove", 10, 560);

    // Tilt/lift feedback: floating ghost rotated 4°, source card dimmed.
    const ghost = document.querySelector<HTMLElement>(".drag-ghost");
    expect(ghost).toBeTruthy();
    expect(ghost!.style.transform).toContain("rotate(4deg)");
    expect(card.classList.contains("dragging")).toBe(true);

    fire(window, "pointerup", 10, 560);
    await settle();

    expect(document.querySelector(".drag-ghost")).toBeNull();
    expect(cardTitles()).toEqual([
      "Скоро (7 дней)",
      "Расписания",
      "Цели",
      "Свежие заметки",
      "Последние чаты",
      "Просрочено",
    ]);
    expect(readPersistedOrder()).toEqual([
      "soon",
      "schedules",
      "objectives",
      "notes",
      "chats",
      "overdue",
    ]);
  });

  it("a restored localStorage order is applied on mount", async () => {
    localStorage.setItem("today-card-order", JSON.stringify(["chats", "notes", "overdue"]));
    await mount();
    // Saved ids first, then the missing ones in default order.
    expect(cardTitles()).toEqual([
      "Последние чаты",
      "Свежие заметки",
      "Просрочено",
      "Скоро (7 дней)",
      "Расписания",
      "Цели",
    ]);
  });

  it("unknown and duplicate ids in storage are ignored, missing ids appended", async () => {
    localStorage.setItem(
      "today-card-order",
      JSON.stringify(["chats", "bogus-card", "overdue", "chats"]),
    );
    await mount();
    expect(cardTitles()).toEqual([
      "Последние чаты",
      "Просрочено",
      "Скоро (7 дней)",
      "Расписания",
      "Цели",
      "Свежие заметки",
    ]);
  });

  it("Escape cancels the drag: order unchanged, nothing persisted", async () => {
    await mount();
    const card = container.querySelector('.today-card[data-card-id="overdue"]')!;
    const head = card.querySelector(".today-card-head")!;

    fire(head, "pointerdown", 10, 10);
    fire(window, "pointermove", 10, 560);
    expect(document.querySelector(".drag-ghost")).toBeTruthy();

    act(() => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });

    expect(document.querySelector(".drag-ghost")).toBeNull();
    expect(cardTitles()).toEqual(DEFAULT_TITLES);
    expect(readPersistedOrder()).toBeNull();
  });

  it("a drag starting on a card action button never starts, and the button still works", async () => {
    const onTab = await mount();
    const btn = container.querySelector(".today-card-all")!;

    fire(btn, "pointerdown", 10, 10);
    fire(window, "pointermove", 10, 560);
    expect(document.querySelector(".drag-ghost")).toBeNull();
    fire(window, "pointerup", 10, 560);

    await click(btn);
    expect(onTab).toHaveBeenCalledWith("harness");
    expect(cardTitles()).toEqual(DEFAULT_TITLES);
  });
});

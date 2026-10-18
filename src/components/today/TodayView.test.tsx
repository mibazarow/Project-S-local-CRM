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

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectDto } from "../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

vi.mock("../api/ipc", () => ({
  listProjects: vi.fn(async () => projects),
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-test" })),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async () => () => {}),
  onTasksChanged: vi.fn(async () => () => {}),
  onSchedulesChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
  onScheduleOpen: vi.fn(async () => () => {}),
  quickCapture: vi.fn(async () => ({ kind: "note" })),
}));

const projects = vi.hoisted(() => [
  {
    id: 1,
    name: "Alpha",
    path: "/p/alpha",
    createdAt: "2026-01-01T00:00:00Z",
    openedAt: null,
    position: 0,
    fileCount: 0,
    taskCount: 0,
    overdueCount: 0,
    hasActiveTasks: false,
    taskColumns: ["a", "b", "c"],
    filesView: "list",
    archived: false,
    archivedAt: null,
  } satisfies ProjectDto,
]);

import QuickCapture from "./QuickCapture";
import { AppProvider } from "../state/AppStore";

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

async function mount(initialTarget: "note" | "task") {
  await act(async () => {
    root.render(
      <AppProvider>
        <QuickCapture initialTarget={initialTarget} onClose={() => {}} />
      </AppProvider>,
    );
  });
}

const segs = () => Array.from(container.querySelectorAll(".segmented .seg"));

describe("QuickCapture segmented target switch", () => {
  it("renders note/task segments with the initial target active", async () => {
    await mount("note");
    expect(segs()).toHaveLength(2);
    expect(segs()[0].className).toBe("seg active");
    expect(segs()[0].textContent).toBe("Заметка");
    expect(segs()[1].className).toBe("seg");
    expect(segs()[1].textContent).toBe("Задача");
    expect(container.querySelector(".capture-project")).toBeNull();
  });

  it("clicking the task segment activates it and shows the project select", async () => {
    await mount("note");
    await act(async () => {
      segs()[1].dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(segs()[0].className).toBe("seg");
    expect(segs()[1].className).toBe("seg active");
    expect(container.querySelector(".capture-project")).not.toBeNull();
  });
});

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectDto } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const projects = vi.hoisted(() => [
  {
    id: 1,
    name: "Alpha",
    path: "/p/alpha",
    createdAt: "2026-01-01T00:00:00Z",
    openedAt: null,
    position: 0,
    fileCount: 1,
    taskCount: 0,
    overdueCount: 0,
    hasActiveTasks: false,
    taskColumns: ["a", "b", "c"],
  } satisfies ProjectDto,
]);

vi.mock("../api/ipc", () => ({
  listProjects: vi.fn(async () => projects),
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-test" })),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async () => () => {}),
  onTasksChanged: vi.fn(async () => () => {}),
  onSchedulesChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
  fsListDir: vi.fn(async () => []),
  tasksList: vi.fn(async () => []),
}));

vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: vi.fn(async () => () => {}),
  }),
}));

import FilesPanel from "./FilesPanel";
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

async function mount() {
  await act(async () => {
    root.render(
      <AppProvider>
        <FilesPanel project={projects[0]} />
      </AppProvider>,
    );
  });
  await act(async () => {
    await Promise.resolve();
  });
}

const segs = () => Array.from(container.querySelectorAll(".center-head .seg"));

describe("FilesPanel segmented tab switch", () => {
  it("renders files/tasks segments with files active and the search box shown", async () => {
    await mount();
    expect(segs()).toHaveLength(2);
    expect(segs()[0].className).toBe("seg active");
    expect(segs()[0].textContent).toBe("Файлы");
    expect(segs()[1].className).toBe("seg");
    expect(segs()[1].textContent).toBe("Задачи");
    expect(container.querySelector(".center-head .search input")).not.toBeNull();
  });

  it("clicking the tasks segment activates it, hides the search box and renders the board", async () => {
    await mount();
    await act(async () => {
      segs()[1].dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(segs()[0].className).toBe("seg");
    expect(segs()[1].className).toBe("seg active");
    expect(container.querySelector(".center-head .search input")).toBeNull();
    expect(container.querySelector(".task-filters")).not.toBeNull();
  });
});

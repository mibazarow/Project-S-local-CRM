import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectDto } from "../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const bus = vi.hoisted(() => ({
  fs: undefined as ((p: { projectId: number; relPath: string }) => void) | undefined,
  tasks: undefined as ((p: { projectId: number }) => void) | undefined,
  schedules: undefined as (() => void) | undefined,
  projects: undefined as (() => void) | undefined,
}));

const projects = vi.hoisted((): ProjectDto[] => [
  {
    id: 1,
    name: "Alpha",
    path: "/p/alpha",
    createdAt: "2026-01-01T00:00:00Z",
    openedAt: null,
    position: 0,
    fileCount: 1,
    taskCount: 2,
    overdueCount: 0,
    hasActiveTasks: false,
    taskColumns: ["a", "b", "c"],
    filesView: "list",
    archived: false,
    archivedAt: null,
  },
]);

vi.mock("../api/ipc", () => ({
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-test" })),
  listProjects: vi.fn(async () => projects),
  projectSetArchived: vi.fn(async () => undefined),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async (cb: (p: { projectId: number; relPath: string }) => void) => {
    bus.fs = cb;
    return () => {
      bus.fs = undefined;
    };
  }),
  onTasksChanged: vi.fn(async (cb: (p: { projectId: number }) => void) => {
    bus.tasks = cb;
    return () => {
      bus.tasks = undefined;
    };
  }),
  onSchedulesChanged: vi.fn(async (cb: () => void) => {
    bus.schedules = cb;
    return () => {
      bus.schedules = undefined;
    };
  }),
  onProjectsChanged: vi.fn(async (cb: () => void) => {
    bus.projects = cb;
    return () => {
      bus.projects = undefined;
    };
  }),
}));

import * as ipc from "../api/ipc";
import { AppProvider, useApp, useAppActions, useAppState } from "./AppStore";

let actionsRenders = 0;
let stateRenders = 0;
let combinedRenders = 0;
let seenActions: unknown[] = [];

function ActionsOnlyProbe() {
  actionsRenders += 1;
  const actions = useAppActions();
  seenActions.push(actions);
  return <button onClick={() => void actions.openProject(1)}>open</button>;
}

function StateOnlyProbe() {
  stateRenders += 1;
  const state = useAppState();
  return <span>{state.projects.length}</span>;
}

function CombinedProbe() {
  combinedRenders += 1;
  const { state } = useApp();
  return <span>{state.activeProjectId}</span>;
}

// Surfaces the active-task indicator flag so a test can assert it survives a
// project-list reload triggered by a task-status event.
function ActiveProbe() {
  const { state } = useApp();
  return <span className="active-flag">{String(state.projects[0]?.hasActiveTasks ?? "")}</span>;
}

let container: HTMLDivElement;
let root: Root;

async function mount() {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root.render(
      <AppProvider>
        <ActionsOnlyProbe />
        <StateOnlyProbe />
        <CombinedProbe />
        <ActiveProbe />
      </AppProvider>,
    );
  });
}

beforeEach(() => {
  actionsRenders = 0;
  stateRenders = 0;
  combinedRenders = 0;
  seenActions = [];
  projects[0].hasActiveTasks = false;
  vi.mocked(ipc.listProjects).mockClear();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("app store split", () => {
  it("initial load populates onboarding, projects and active selection", async () => {
    await mount();
    expect(ipc.onboardingStatus).toHaveBeenCalledTimes(1);
    expect(ipc.listProjects).toHaveBeenCalled();
    expect(container.textContent).toContain("1");
    expect(container.querySelectorAll("span")[1].textContent).toBe("1");
  });

  it("actions identity is stable across data updates", async () => {
    await mount();
    await act(async () => bus.projects?.());
    await act(async () => bus.tasks?.({ projectId: 1 }));
    await act(async () => bus.schedules?.());
    expect(seenActions.length).toBe(1);
    expect(new Set(seenActions).size).toBe(1);
  });

  it("actions-only consumers do not re-render on data updates", async () => {
    await mount();
    expect(actionsRenders).toBe(1);
    await act(async () => bus.projects?.());
    await act(async () => bus.tasks?.({ projectId: 1 }));
    await act(async () => bus.fs?.({ projectId: 1, relPath: "x.md" }));
    await act(async () => bus.schedules?.());
    await act(async () => {
      container.querySelector("button")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(actionsRenders).toBe(1);
  });

  it("data consumers still re-render when data updates", async () => {
    await mount();
    const before = stateRenders;
    await act(async () => bus.projects?.());
    expect(stateRenders).toBeGreaterThan(before);
    expect(combinedRenders).toBeGreaterThan(0);
  });

  it("refresh on backend events reloads only the project list", async () => {
    await mount();
    vi.mocked(ipc.listProjects).mockClear();
    await act(async () => bus.fs?.({ projectId: 1, relPath: "x.md" }));
    await act(async () => bus.tasks?.({ projectId: 1 }));
    await act(async () => bus.projects?.());
    await act(async () => bus.schedules?.());
    expect(ipc.listProjects).toHaveBeenCalledTimes(3);
  });

  it("task-status event reloads the list and carries the active-task flag", async () => {
    await mount();
    expect(container.querySelector(".active-flag")!.textContent).toBe("false");
    projects[0].hasActiveTasks = true;
    vi.mocked(ipc.listProjects).mockClear();
    await act(async () => bus.tasks?.({ projectId: 1 }));
    expect(ipc.listProjects).toHaveBeenCalledTimes(1);
    expect(container.querySelector(".active-flag")!.textContent).toBe("true");
  });

  it("archiveProject calls the IPC and reloads the project list", async () => {
    await mount();
    const acts = seenActions[0] as {
      archiveProject: (id: number, archived: boolean) => Promise<void>;
    };
    vi.mocked(ipc.listProjects).mockClear();
    await act(async () => {
      await acts.archiveProject(1, true);
    });
    expect(ipc.projectSetArchived).toHaveBeenCalledWith(1, true);
    expect(ipc.listProjects).toHaveBeenCalledTimes(1);
    await act(async () => {
      await acts.archiveProject(1, false);
    });
    expect(ipc.projectSetArchived).toHaveBeenCalledWith(1, false);
  });
});

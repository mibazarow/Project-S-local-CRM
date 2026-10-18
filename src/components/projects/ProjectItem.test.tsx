import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { ProjectDto } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

import ProjectItem from "./ProjectItem";

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

const makeProject = (overrides: Partial<ProjectDto> = {}): ProjectDto => ({
  id: 1,
  name: "example.ru",
  path: "/wd/Projects/example.ru",
  createdAt: "2026-01-01 09:00:00",
  openedAt: null,
  position: 1,
  fileCount: 3,
  taskCount: 5,
  overdueCount: 0,
  hasActiveTasks: false,
  taskColumns: ["Новые", "В работе", "Готово"],
  filesView: "list",
  ...overrides,
});

async function mount(project: ProjectDto) {
  await act(async () => {
    root.render(
      <ProjectItem
        project={project}
        active={false}
        dragging={false}
        onOpen={() => {}}
        onRename={() => {}}
        onPointerDown={() => {}}
      />,
    );
  });
}

describe("ProjectItem active-task indicator", () => {
  it("shows the Play icon with a Russian tooltip and accessible label when a task is in progress", async () => {
    await mount(makeProject({ hasActiveTasks: true }));
    const icon = container.querySelector(".active-tasks");
    expect(icon).not.toBeNull();
    expect(icon!.getAttribute("aria-label")).toBe("Есть задачи в работе");
    expect(icon!.querySelector("title")?.textContent).toBe("Есть задачи в работе");
  });

  it("renders no Play icon when the project has no progress task", async () => {
    await mount(makeProject({ hasActiveTasks: false }));
    expect(container.querySelector(".active-tasks")).toBeNull();
  });
});

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const actions = vi.hoisted(() => ({
  createProject: vi.fn(async () => {}),
  moveProject: vi.fn(async () => {}),
  openProject: vi.fn(async () => {}),
  renameProject: vi.fn(async () => {}),
  clearNewProject: vi.fn(),
}));

vi.mock("../../state/AppStore", () => ({
  useApp: () => ({
    state: { projects: [], activeProjectId: null, newProjectRequested: false },
    actions,
  }),
}));

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import ProjectList from "./ProjectList";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

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
  vi.clearAllMocks();
});

async function mount() {
  await act(async () => {
    root.render(<ProjectList />);
  });
}

describe("ProjectList new-project button (Q-21)", () => {
  it("renders a labeled circular-plus create control with the Russian text and title", async () => {
    await mount();
    const btn = container.querySelector<HTMLButtonElement>(".add-project-btn");
    expect(btn).not.toBeNull();
    expect(btn!.getAttribute("title")).toBe("Новый проект");
    expect(btn!.textContent).toContain("Новый проект");
    expect(btn!.querySelector(".add-project-plus svg")).not.toBeNull();
  });

  it("clicking the create control opens the inline name input (handler unchanged)", async () => {
    await mount();
    expect(container.querySelector(".add-input")).toBeNull();
    const btn = container.querySelector<HTMLButtonElement>(".add-project-btn")!;
    await act(async () => {
      btn.click();
    });
    expect(container.querySelector(".add-input")).not.toBeNull();
  });
});

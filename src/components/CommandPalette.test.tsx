import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
  notesTree: vi.fn(async () => [] as unknown[]),
  harnessListSessions: vi.fn(async () => [] as unknown[]),
  globalSearch: vi.fn(async () => [] as unknown[]),
  harnessCreateSession: vi.fn(async () => ({ id: 99 })),
  reindexAll: vi.fn(async () => 0),
  useApp: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../api/ipc", () => h);
vi.mock("../state/AppStore", () => ({ useApp: h.useApp }));

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { FsEntry, GlobalHit, ProjectDto } from "../api/types";
import CommandPalette from "./CommandPalette";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

window.HTMLElement.prototype.scrollIntoView = vi.fn();

const project = (id: number, name: string): ProjectDto => ({
  id,
  name,
  path: `/p${id}`,
  createdAt: "2026-01-01T00:00:00Z",
  openedAt: null,
  position: 0,
  fileCount: 3,
  taskCount: 0,
  overdueCount: 0,
  hasActiveTasks: false,
  taskColumns: ["Новые", "В работе", "Готово"],
  filesView: "list",
});

const noteEntry = (relPath: string): FsEntry => ({
  name: relPath.split("/").pop() ?? relPath,
  relPath,
  kind: "file",
  ext: "md",
  size: 1,
  modified: 0,
  isMarkdown: true,
  isText: true,
  isImage: false,
});

const gHit = (over: Partial<GlobalHit> & Pick<GlobalHit, "kind" | "id" | "title">): GlobalHit => ({
  subtitle: "",
  snippet: "",
  score: 1,
  ...over,
});

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.useFakeTimers();
  h.useApp.mockReturnValue({
    state: { projects: [project(1, "Example One"), project(2, "Example Two")] },
    actions: {
      openProject: vi.fn(),
      requestFile: vi.fn(),
      requestTask: vi.fn(),
      requestNote: vi.fn(),
      requestSession: vi.fn(),
      requestSettings: vi.fn(),
      requestNewProject: vi.fn(),
      requestNewNote: vi.fn(),
    },
  });
  h.notesTree.mockResolvedValue([noteEntry("example.md"), noteEntry("example2.md")]);
  h.harnessListSessions.mockResolvedValue([
    { id: 7, title: "Example chat" },
    { id: 8, title: "Example two chat" },
  ]);
  h.globalSearch.mockResolvedValue([]);
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
  vi.clearAllMocks();
});

const renderPalette = async () => {
  await act(async () => {
    root.render(<CommandPalette onTab={vi.fn()} onClose={vi.fn()} />);
  });
};

const typeQuery = async (value: string) => {
  const input = container.querySelector("input")!;
  const setter = Object.getOwnPropertyDescriptor(
    window.HTMLInputElement.prototype,
    "value",
  )!.set!;
  act(() => {
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => {
    vi.advanceTimersByTime(250);
  });
};

const groups = () =>
  [...container.querySelectorAll(".palette-group")].map((g) => ({
    title: g.querySelector(".palette-group-title")?.textContent,
    names: [...g.querySelectorAll(".palette-item-name")].map((n) => n.textContent),
  }));

describe("CommandPalette search groups", () => {
  it("does not call globalSearch for queries shorter than two chars", async () => {
    await renderPalette();
    await typeQuery("e");
    expect(h.globalSearch).not.toHaveBeenCalled();
  });

  it("keeps the Поиск group in backend order without rescoring", async () => {
    h.globalSearch.mockResolvedValue([
      gHit({ kind: "project", id: 10, title: "xexample" }),
      gHit({ kind: "project", id: 11, title: "exampleX" }),
    ]);
    await renderPalette();
    await typeQuery("example");
    const search = groups().find((g) => g.title === "Поиск");
    expect(search?.names).toEqual(["xexample", "exampleX"]);
  });

  it("drops static entries that the Поиск group already lists", async () => {
    h.globalSearch.mockResolvedValue([
      gHit({ kind: "project", id: 1, title: "Example One" }),
      gHit({ kind: "note", id: 2, title: "example.md", relPath: "example.md" }),
      gHit({ kind: "chat", id: 3, title: "Example chat", sessionId: 7 }),
    ]);
    await renderPalette();
    await typeQuery("example");
    expect(groups()).toEqual([
      { title: "Поиск", names: ["Example One", "example.md", "Example chat"] },
      { title: "Проекты", names: ["Example Two"] },
      { title: "Заметки", names: ["example2"] },
      { title: "Чаты харнеса", names: ["Example two chat"] },
    ]);
  });

  it("falls back to local static matches when the backend returns nothing", async () => {
    h.globalSearch.mockResolvedValue([]);
    await renderPalette();
    await typeQuery("example");
    const titles = groups().map((g) => g.title);
    expect(titles).not.toContain("Поиск");
    const projects = groups().find((g) => g.title === "Проекты");
    expect(projects?.names).toEqual(["Example One", "Example Two"]);
  });

  it("caps the Поиск group at eight hits", async () => {
    h.globalSearch.mockResolvedValue(
      Array.from({ length: 10 }, (_, i) => gHit({ kind: "project", id: 100 + i, title: `example ${i}` })),
    );
    await renderPalette();
    await typeQuery("example");
    const search = groups().find((g) => g.title === "Поиск");
    expect(search?.names.length).toBe(8);
  });

  it("clears hits synchronously when the raw query drops below two chars", async () => {
    h.globalSearch.mockResolvedValue([gHit({ kind: "project", id: 10, title: "example hit" })]);
    await renderPalette();
    await typeQuery("example");
    expect(groups().some((g) => g.title === "Поиск")).toBe(true);
    const input = container.querySelector("input")!;
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      "value",
    )!.set!;
    act(() => {
      setter.call(input, "e");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(groups().some((g) => g.title === "Поиск")).toBe(false);
  });
});

describe("CommandPalette renamed section label", () => {
  it("the Перейти group lists «Заметки и файлы» instead of «Блокнот»", async () => {
    await renderPalette();
    await typeQuery("файлы");
    const nav = groups().find((g) => g.title === "Перейти");
    expect(nav?.names).toContain("Заметки и файлы");
    const allNames = groups().flatMap((g) => g.names);
    expect(allNames).not.toContain("Блокнот");
  });
});

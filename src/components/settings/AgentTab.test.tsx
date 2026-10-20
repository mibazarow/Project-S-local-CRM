import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { IndexStats, ProjectIndexStats } from "../../api/types";
import type { DebouncedConfig } from "./useDebouncedConfig";
import type { AppForm } from "./forms";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));

import AgentTab from "./AgentTab";
import ConfirmDialogHost from "../shared/ConfirmDialogHost";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const stats = (o: Partial<IndexStats>): IndexStats => ({
  filesSeen: 0,
  filesIndexed: 0,
  filesSkipped: 0,
  filesRemoved: 0,
  errors: [],
  ...o,
});

const result = (projectId: number, s: Partial<IndexStats>): ProjectIndexStats => ({
  projectId,
  stats: stats(s),
});

let reindexAllResult: ProjectIndexStats[];
let reindexHardResult: ProjectIndexStats[];
let reindexHardDeferred: Promise<ProjectIndexStats[]> | null;

const invoked = (cmd: string) => invokeMock.mock.calls.filter((c) => c[0] === cmd);

const appStub: DebouncedConfig<AppForm> = {
  form: {
    autopilot: false,
    sessionTtlDays: "30",
    tmpRetentionDays: "7",
    logRetentionDays: "30",
    backupIntervalDays: "1",
    backupKeep: "5",
  },
  updateForm: () => {},
  reload: () => {},
  flush: async () => {},
  focusField: () => () => {},
  blurField: () => {},
  focusedRef: { current: null },
};

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  reindexAllResult = [];
  reindexHardResult = [];
  reindexHardDeferred = null;
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "read_config":
        return { maxFileSizeBytes: 10485760, maxTextChars: 1000000, excludeDirs: [] };
      case "embed_status":
        return { backend: "noop", dim: 768, available: false, modelPresent: false, reason: null };
      case "reindex_all":
        return reindexAllResult;
      case "reindex_hard":
        return reindexHardDeferred ?? reindexHardResult;
      default:
        return undefined;
    }
  });
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
      <>
        <AgentTab active error={null} setError={() => {}} app={appStub} />
        <ConfirmDialogHost />
      </>,
    );
  });
  await act(async () => {});
}

const text = () => container.textContent ?? "";

const btn = (label: string) =>
  Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
    (b) => b.textContent?.trim() === label,
  )!;

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

describe("AgentTab reindex stats", () => {
  it("says the index is current instead of «0 из N» when nothing changed", async () => {
    reindexAllResult = [result(1, { filesSeen: 101, filesSkipped: 101 })];
    await mount();
    await click(btn("Переиндексировать"));
    expect(text()).toContain("Индекс актуален");
    expect(text()).not.toContain("0 из 101");
    expect(text()).not.toContain("индексировано");
  });

  it("shows honest checked/updated/unchanged/removed counters", async () => {
    reindexAllResult = [
      result(1, { filesSeen: 7, filesIndexed: 2, filesSkipped: 4, filesRemoved: 1 }),
      result(2, { filesSeen: 3, filesSkipped: 3 }),
    ];
    await mount();
    await click(btn("Переиндексировать"));
    expect(text()).toContain("Проверено 10, обновлено 2, без изменений 7, удалено 1");
  });

  it("lists the first errors and caps the rest", async () => {
    reindexAllResult = [
      result(1, {
        filesSeen: 5,
        filesIndexed: 5,
        errors: ["a.docx: broken", "b.docx: broken", "c.md: embed: down", "d.md: embed: down", "e.md: x"],
      }),
    ];
    await mount();
    await click(btn("Переиндексировать"));
    expect(text()).toContain("Ошибки: a.docx: broken; b.docx: broken; c.md: embed: down (+2)");
    expect(text()).not.toContain("d.md");
  });
});

describe("AgentTab hard rebuild", () => {
  it("asks for confirmation and only then calls reindex_hard", async () => {
    reindexHardResult = [result(1, { filesSeen: 4, filesIndexed: 4 })];
    await mount();
    await click(btn("Пересобрать индекс полностью"));
    const dialog = document.body.querySelector(".confirm-dialog")!;
    expect(dialog.textContent).toContain("Пересобрать индекс полностью?");
    expect(dialog.textContent).toContain("очищен");
    expect(invoked("reindex_hard")).toHaveLength(0);
    await click(dialog.querySelector<HTMLButtonElement>(".ghost-btn")!);
    expect(invoked("reindex_hard")).toHaveLength(0);

    await click(btn("Пересобрать индекс полностью"));
    const dialog2 = document.body.querySelector(".confirm-dialog")!;
    await click(dialog2.querySelector<HTMLButtonElement>(".primary-btn")!);
    expect(invoked("reindex_hard")).toHaveLength(1);
    expect(text()).toContain("Проверено 4, обновлено 4, без изменений 0, удалено 0");
  });

  it("shows a busy state while the rebuild runs and updates the note after", async () => {
    let resolveHard: (r: ProjectIndexStats[]) => void = () => {};
    reindexHardDeferred = new Promise((resolve) => {
      resolveHard = resolve;
    });
    await mount();
    await click(btn("Пересобрать индекс полностью"));
    await click(document.body.querySelector(".confirm-dialog")!.querySelector<HTMLButtonElement>(".primary-btn")!);
    expect(btn("Пересборка…")).toBeDefined();
    expect(btn("Переиндексировать").disabled).toBe(true);
    resolveHard([result(1, { filesSeen: 2, filesIndexed: 2 })]);
    await act(async () => {});
    expect(text()).toContain("Проверено 2, обновлено 2, без изменений 0, удалено 0");
    expect(btn("Пересобрать индекс полностью").disabled).toBe(false);
  });
});

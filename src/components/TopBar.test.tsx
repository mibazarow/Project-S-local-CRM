import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as ipc from "../api/ipc";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const NOW = 1_000_000;

const bus = vi.hoisted(() => ({
  progress: undefined as
    | ((p: { done: number; total: number; phase: string }) => void)
    | undefined,
  done: undefined as ((p: { done: number; total: number }) => void) | undefined,
  status: {
    running: false,
    done: 0,
    total: 0,
    phase: "",
    nextRunAt: 0,
    lastRunAt: null as number | null,
  },
}));

vi.mock("../api/ipc", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api/ipc")>()),
  reindexStatus: vi.fn(async () => bus.status),
  onReindexProgress: vi.fn(async (cb: (p: { done: number; total: number; phase: string }) => void) => {
    bus.progress = cb;
    return () => {
      bus.progress = undefined;
    };
  }),
  onReindexDone: vi.fn(async (cb: (p: { done: number; total: number }) => void) => {
    bus.done = cb;
    return () => {
      bus.done = undefined;
    };
  }),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ startDragging: async () => {} }),
}));

import TopBar from "./TopBar";

let container: HTMLDivElement;
let root: Root;

const reindexBtn = () =>
  container.querySelector<HTMLButtonElement>(".reindex-btn")!;

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW);
  bus.progress = undefined;
  bus.done = undefined;
  bus.status = {
    running: false,
    done: 0,
    total: 0,
    phase: "",
    nextRunAt: NOW + 300_000,
    lastRunAt: null,
  };
  vi.mocked(ipc.reindexStatus).mockClear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

async function mount() {
  await act(async () => {
    root.render(<TopBar tab="projects" onTab={() => {}} onSettings={() => {}} onSearch={() => {}} />);
  });
}

describe("TopBar event-driven reindex status", () => {
  it("fetches the status once on mount and never on an interval", async () => {
    await mount();
    expect(ipc.reindexStatus).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(ipc.reindexStatus).toHaveBeenCalledTimes(1);
    // The countdown display still ticks from the known nextRunAt.
    expect(reindexBtn().textContent).toBe("4:00");
  });

  it("a reindex-progress event updates the UI without any status IPC", async () => {
    await mount();
    await act(async () => {
      bus.progress?.({ done: 2, total: 5, phase: "Заметки" });
    });
    expect(reindexBtn().textContent).toBe("2/5");
    expect(reindexBtn().getAttribute("title")).toBe("Переиндексация поиска: Заметки");
    expect(ipc.reindexStatus).toHaveBeenCalledTimes(1);
  });

  it("a reindex-done event resets the countdown without any status IPC", async () => {
    await mount();
    await act(async () => {
      bus.progress?.({ done: 5, total: 5, phase: "Память" });
    });
    expect(reindexBtn().textContent).toBe("5/5");
    await act(async () => {
      bus.done?.({ done: 5, total: 5 });
    });
    expect(reindexBtn().textContent).toBe("5:00");
    expect(ipc.reindexStatus).toHaveBeenCalledTimes(1);
  });

  it("refetches the status when the window regains focus", async () => {
    await mount();
    await act(async () => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(ipc.reindexStatus).toHaveBeenCalledTimes(2);
  });
});

describe("TopBar section labels", () => {
  it("the notes tab is labeled «Заметки и файлы»", async () => {
    await mount();
    const labels = [...container.querySelectorAll(".tab")].map((b) => b.textContent);
    expect(labels).toContain("Заметки и файлы");
    expect(labels).not.toContain("Блокнот");
  });
});

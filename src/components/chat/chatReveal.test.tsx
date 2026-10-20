import { act, useRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

// Render the raw revealed slice so a test can read exactly how much of the
// answer is on screen at any moment.
vi.mock("./MarkdownMessage", () => ({
  MarkdownMessage: ({ text }: { text: string }) => <div className="bubble md">{text}</div>,
}));

import { ChatTranscript, type ChatItem, type Slice } from "./chatShared";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

// Deterministic requestAnimationFrame driver: RevealAnswer schedules one rAF per
// reveal step, so flushing the queue advances exactly one frame and honours
// cancelAnimationFrame on unmount / text change.
let rafMap: Map<number, FrameRequestCallback>;
let rafSeq = 0;

function flushFrame() {
  const entries = [...rafMap.entries()];
  rafMap.clear();
  for (const [, cb] of entries) cb(0);
}

// Advance frames until the reveal stops scheduling (settled) or a cap is hit.
function revealFrames(cap = 200) {
  for (let i = 0; i < cap && rafMap.size > 0; i++) act(() => flushFrame());
}

function setReducedMotion(on: boolean) {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: on,
    media: query,
    onchange: null,
    addEventListener() {},
    removeEventListener() {},
    addListener() {},
    removeListener() {},
    dispatchEvent: () => false,
  }));
}

function runSlice(text: string): Slice {
  return {
    loaded: true,
    items: [],
    run: { text, reasoning: "", steps: [], compactions: [], error: null, local: true },
    confirmations: [],
    sendError: null,
    queue: [],
    ready: false,
  };
}

function agentItem(text: string, extra: Partial<ChatItem> = {}): ChatItem {
  return {
    key: 100,
    role: "agent",
    text,
    reasoning: "",
    attachments: [],
    messageIds: [],
    pinned: false,
    steps: [],
    compactions: [],
    note: null,
    error: null,
    ...extra,
  };
}

function settledSlice(item: ChatItem): Slice {
  return { loaded: true, items: [item], run: null, confirmations: [], sendError: null, queue: [], ready: false };
}

function Transcript({ slice }: { slice: Slice }) {
  const listRef = useRef<HTMLDivElement>(null);
  return (
    <ChatTranscript
      slice={slice}
      listRef={listRef}
      empty="Пусто"
      projectId={1}
      chatId={10}
    />
  );
}

let container: HTMLDivElement;
let root: Root;

const shown = () => container.querySelector(".bubble.md")?.textContent ?? "";

beforeEach(() => {
  rafMap = new Map();
  rafSeq = 0;
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
    const id = ++rafSeq;
    rafMap.set(id, cb);
    return id;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => {
    rafMap.delete(id);
  });
  setReducedMotion(false);
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

describe("progressive answer reveal (SPEC 6.0, Q22)", () => {
  it("shows a streamed delta immediately, without waiting for a frame", async () => {
    await act(async () => {
      root.render(<Transcript slice={runSlice("Привет!")} />);
    });
    // A small streamed delta is fully on screen at once: no typewriter, no rAF.
    expect(shown()).toBe("Привет!");
    expect(rafMap.size).toBe(0);
  });

  it("reveals a single large chunk progressively and settles to the full answer", async () => {
    const answer = "Л".repeat(120);
    await act(async () => {
      root.render(<Transcript slice={runSlice(answer)} />);
    });
    // Not dumped whole: the first frame shows only a step of the block.
    expect(shown().length).toBeLessThan(answer.length);
    act(() => flushFrame());
    const first = shown().length;
    expect(first).toBeGreaterThan(0);
    expect(first).toBeLessThan(answer.length);
    revealFrames();
    // Final rendered text equals the whole answer.
    expect(shown()).toBe(answer);
  });

  it("never duplicates the answer while revealing", async () => {
    const answer = "НАЧАЛО " + "·".repeat(100) + " КОНЕЦ";
    await act(async () => {
      root.render(<Transcript slice={runSlice(answer)} />);
    });
    act(() => flushFrame());
    act(() => flushFrame());
    const mid = shown();
    // At every point the on-screen text is a prefix of the answer, never a
    // re-typed or doubled copy of it.
    expect(answer.startsWith(mid)).toBe(true);
    revealFrames();
    const full = shown();
    expect(full).toBe(answer);
    expect((full.match(/НАЧАЛО/g) ?? []).length).toBe(1);
    expect((full.match(/КОНЕЦ/g) ?? []).length).toBe(1);
  });

  it("continues the reveal onto the settled answer after done without duplicating", async () => {
    const answer = "Z".repeat(120);
    await act(async () => {
      root.render(<Transcript slice={runSlice(answer)} />);
    });
    act(() => flushFrame());
    act(() => flushFrame());
    const mid = shown();
    expect(mid.length).toBeGreaterThan(0);
    expect(mid.length).toBeLessThan(answer.length);

    // done: the run clears and the answer settles as a live item (no message
    // ids, no note/error). The settled bubble seeds from what the run bubble
    // already showed, so the reveal continues instead of restarting.
    await act(async () => {
      root.render(<Transcript slice={settledSlice(agentItem(answer))} />);
    });
    expect(shown().length).toBeGreaterThanOrEqual(mid.length);
    revealFrames();
    expect(shown()).toBe(answer);
    expect((shown().match(/Z/g) ?? []).length).toBe(answer.length);
  });

  it("settles a stopped answer to the full text instantly", async () => {
    const answer = "Q".repeat(120);
    const item = agentItem(answer, { note: "Работа остановлена." });
    await act(async () => {
      root.render(<Transcript slice={settledSlice(item)} />);
    });
    // A stopped (note-bearing) row is not a live answer: no reveal, full text
    // at once, and no frame is scheduled.
    expect(shown()).toBe(answer);
    expect(rafMap.size).toBe(0);
  });

  it("settles a provider-silence timeout error to the full text instantly", async () => {
    // Q23: the timeout path sets the run item's `error` (not `note`), so the
    // reveal settles immediately — no typewriter over a failed answer.
    const answer = "T".repeat(120);
    const item = agentItem(answer, {
      error: "Провайдер не отвечает 180 секунд — проверьте баланс, API-ключ и статус провайдера.",
      retryable: true,
    });
    await act(async () => {
      root.render(<Transcript slice={settledSlice(item)} />);
    });
    expect(shown()).toBe(answer);
    expect(rafMap.size).toBe(0);
  });

  it("shows the answer instantly under prefers-reduced-motion", async () => {
    setReducedMotion(true);
    const answer = "R".repeat(120);
    await act(async () => {
      root.render(<Transcript slice={runSlice(answer)} />);
    });
    expect(shown()).toBe(answer);
    expect(rafMap.size).toBe(0);
  });
});

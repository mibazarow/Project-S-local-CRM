import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

type Handler = (p: unknown) => void;
const bus = vi.hoisted(() => ({
  agent: undefined as Handler | undefined,
  confirm: undefined as Handler | undefined,
  file: undefined as Handler | undefined,
  compaction: undefined as Handler | undefined,
  proposal: undefined as Handler | undefined,
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

vi.mock("../api/ipc", () => ({
  onAgentEvent: vi.fn(async (cb: Handler) => {
    bus.agent = cb;
    return () => {};
  }),
  onAgentConfirmation: vi.fn(async (cb: Handler) => {
    bus.confirm = cb;
    return () => {};
  }),
  onAgentFile: vi.fn(async (cb: Handler) => {
    bus.file = cb;
    return () => {};
  }),
  onAgentCompaction: vi.fn(async (cb: Handler) => {
    bus.compaction = cb;
    return () => {};
  }),
  onSkillProposal: vi.fn(async (cb: Handler) => {
    bus.proposal = cb;
    return () => {};
  }),
  sendProjectMessage: vi.fn(async () => 10),
  sendSessionMessage: vi.fn(async () => 20),
  sendNotesMessage: vi.fn(async () => undefined),
  cancelRun: vi.fn(async () => true),
  cancelSessionRun: vi.fn(async () => true),
  cancelNotesRun: vi.fn(async () => true),
}));

import * as ipc from "../api/ipc";
import {
  NOTES_KEY,
  dispatch,
  ensureSubscribed,
  flushHeldQueue,
  getSlice,
  openChat,
  projectKey,
  requestStop,
  resetChatRunStore,
  sessionKey,
  useChatReady,
  useChatRunning,
} from "./chatRunStore";
import ProjectItem from "../components/projects/ProjectItem";
import type { ProjectDto } from "../api/types";

const delta = (p: unknown) => act(() => bus.agent?.(p));

function project(id: number): ProjectDto {
  return {
    id,
    name: `p${id}`,
    path: `/p/${id}`,
    createdAt: "",
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
  };
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  resetChatRunStore();
  ensureSubscribed();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  resetChatRunStore();
});

describe("chatRunStore", () => {
  it("keeps streaming into the store with no React subscriber attached (unmount-safe)", () => {
    delta({ projectId: 1, chatId: 10, event: { type: "textDelta", text: "Hel" } });
    delta({ projectId: 1, chatId: 10, event: { type: "textDelta", text: "lo" } });
    expect(getSlice(projectKey(1))?.run?.text).toBe("Hello");
  });

  it("runs two chats concurrently with independent streams", () => {
    delta({ projectId: 1, chatId: 10, event: { type: "textDelta", text: "A1" } });
    delta({ sessionId: 2, chatId: 20, event: { type: "textDelta", text: "B1" } });
    delta({ projectId: 1, chatId: 10, event: { type: "textDelta", text: "A2" } });
    expect(getSlice(projectKey(1))?.run?.text).toBe("A1A2");
    expect(getSlice(sessionKey(2))?.run?.text).toBe("B1");
    // A done on one chat leaves the other's run live.
    delta({ projectId: 1, chatId: 10, event: { type: "done", answer: "A1A2", reason: "finish", reasoning: null } });
    expect(getSlice(projectKey(1))?.run).toBeNull();
    expect(getSlice(sessionKey(2))?.run?.text).toBe("B1");
  });

  it("a background completion is visible on return and marks the chat ready", () => {
    // The chat is not the open one, so finishing it sets the ready flag and
    // persists the final answer for when the user comes back.
    delta({ projectId: 5, chatId: 50, event: { type: "textDelta", text: "q" } });
    dispatch({ type: "send", key: projectKey(5), text: "hi", attachments: [] });
    delta({ projectId: 5, chatId: 50, event: { type: "done", answer: "final", reason: "finish", reasoning: null } });
    const slice = getSlice(projectKey(5));
    expect(slice?.run).toBeNull();
    expect(slice?.ready).toBe(true);
    const last = slice?.items[slice.items.length - 1];
    expect(last?.role).toBe("agent");
    expect(last?.text).toContain("final");
    // Opening the chat clears the ready flag.
    openChat(projectKey(5));
    expect(getSlice(projectKey(5))?.ready).toBe(false);
  });

  it("routes notes events to the fixed notes key and marks it ready in the background", () => {
    delta({ notes: true, chatId: 99, event: { type: "textDelta", text: "n" } });
    expect(getSlice(NOTES_KEY)?.run?.text).toBe("n");
    delta({ notes: true, chatId: 99, event: { type: "done", answer: "n", reason: "finish", reasoning: null } });
    expect(getSlice(NOTES_KEY)?.ready).toBe(true);
  });

  it("routes agent-file events back through the chat id learned from agent events", () => {
    delta({ projectId: 7, chatId: 70, event: { type: "textDelta", text: "x" } });
    act(() =>
      bus.file?.({ chatId: 70, messageId: 1, name: "a.png", path: "p/a.png", kind: "image", isImage: true, caption: "файл" }),
    );
    const items = getSlice(projectKey(7))?.items ?? [];
    expect(items.some((it) => it.synthetic && it.messageIds.includes(1))).toBe(true);
  });

  it("flushes a queued message as one sent turn after the run finishes, without duplicating it", async () => {
    vi.useFakeTimers();
    try {
      openChat(projectKey(6));
      dispatch({ type: "send", key: projectKey(6), text: "first", attachments: [] });
      dispatch({ type: "enqueue", key: projectKey(6), text: "second" });
      expect(getSlice(projectKey(6))?.queue).toEqual(["second"]);
      delta({ projectId: 6, chatId: 60, event: { type: "done", answer: "ans", reason: "finish", reasoning: null } });
      // The queue empties the moment the run ends; the combined send is deferred.
      expect(getSlice(projectKey(6))?.queue).toEqual([]);
      const userTurns = () =>
        (getSlice(projectKey(6))?.items ?? []).filter((it) => it.role === "user").map((it) => it.text);
      expect(userTurns()).toEqual(["first"]);
      await act(async () => {
        vi.advanceTimersByTime(400);
      });
      // The queued text re-arrives as exactly one normal user turn — no duplicate.
      expect(userTurns()).toEqual(["first", "second"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("a cancelled run holds the queue and never auto-sends it", async () => {
    vi.useFakeTimers();
    try {
      openChat(projectKey(8));
      dispatch({ type: "send", key: projectKey(8), text: "first", attachments: [] });
      dispatch({ type: "enqueue", key: projectKey(8), text: "second" });
      delta({ projectId: 8, chatId: 80, event: { type: "done", answer: "part", reason: "cancelled", reasoning: null } });
      expect(getSlice(projectKey(8))?.queue).toEqual(["second"]);
      const userTurns = () =>
        (getSlice(projectKey(8))?.items ?? []).filter((it) => it.role === "user").map((it) => it.text);
      await act(async () => {
        vi.advanceTimersByTime(1000);
      });
      expect(getSlice(projectKey(8))?.queue).toEqual(["second"]);
      expect(userTurns()).toEqual(["first"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("flushHeldQueue sends the held queue as one turn and is a no-op while a run is live", async () => {
    vi.useFakeTimers();
    try {
      openChat(projectKey(9));
      dispatch({ type: "send", key: projectKey(9), text: "first", attachments: [] });
      dispatch({ type: "enqueue", key: projectKey(9), text: "second" });
      delta({ projectId: 9, chatId: 90, event: { type: "done", answer: "part", reason: "cancelled", reasoning: null } });
      act(() => flushHeldQueue(projectKey(9)));
      expect(getSlice(projectKey(9))?.queue).toEqual([]);
      const userTurns = () =>
        (getSlice(projectKey(9))?.items ?? []).filter((it) => it.role === "user").map((it) => it.text);
      await act(async () => {
        vi.advanceTimersByTime(400);
      });
      expect(userTurns()).toEqual(["first", "second"]);
      // While a run is live the flush does nothing: the text stays queued.
      dispatch({ type: "enqueue", key: projectKey(9), text: "third" });
      act(() => flushHeldQueue(projectKey(9)));
      expect(getSlice(projectKey(9))?.queue).toEqual(["third"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("requestStop marks the run stopping until the backend confirms, then clears it", async () => {
    openChat(projectKey(11));
    dispatch({ type: "send", key: projectKey(11), text: "q", attachments: [] });
    act(() => requestStop(projectKey(11)));
    expect(ipc.cancelRun).toHaveBeenCalledWith(11);
    expect(getSlice(projectKey(11))?.stopping).toBe(true);
    await act(async () => {
      await Promise.resolve();
    });
    expect(getSlice(projectKey(11))?.stopping).toBe(true);
    delta({ projectId: 11, chatId: 110, event: { type: "done", answer: "a", reason: "cancelled", reasoning: null } });
    expect(getSlice(projectKey(11))?.stopping).toBe(false);
  });

  it("requestStop clears stopping right away when no run is active", async () => {
    vi.mocked(ipc.cancelSessionRun).mockResolvedValueOnce(false);
    openChat(sessionKey(12));
    act(() => requestStop(sessionKey(12)));
    expect(ipc.cancelSessionRun).toHaveBeenCalledWith(12);
    expect(getSlice(sessionKey(12))?.stopping).toBe(true);
    await act(async () => {
      await Promise.resolve();
    });
    expect(getSlice(sessionKey(12))?.stopping).toBe(false);
  });

  it("requestStop on the notes chat cancels the notes run", async () => {
    openChat(NOTES_KEY);
    act(() => requestStop(NOTES_KEY));
    expect(ipc.cancelNotesRun).toHaveBeenCalledTimes(1);
    expect(getSlice(NOTES_KEY)?.stopping).toBe(true);
  });
});

describe("chat list indicators", () => {
  it("shows a running dot while a run is live and a ready dot after a background finish", async () => {
    await act(async () => {
      root.render(<ProjectItem project={project(3)} active={false} dragging={false} onOpen={() => {}} onRename={() => {}} onArchive={() => {}} onPointerDown={() => {}} />);
    });
    expect(container.querySelector(".chat-status.running")).toBeNull();
    expect(container.querySelector(".chat-status.ready")).toBeNull();

    delta({ projectId: 3, chatId: 30, event: { type: "textDelta", text: "hi" } });
    expect(container.querySelector(".chat-status.running")).not.toBeNull();

    delta({ projectId: 3, chatId: 30, event: { type: "done", answer: "hi", reason: "finish", reasoning: null } });
    expect(container.querySelector(".chat-status.running")).toBeNull();
    expect(container.querySelector(".chat-status.ready")).not.toBeNull();
  });

  it("useChatRunning/useChatReady reflect the store for the given key", () => {
    let running = false;
    let ready = false;
    function Probe() {
      running = useChatRunning(projectKey(4));
      ready = useChatReady(projectKey(4));
      return null;
    }
    act(() => root.render(<Probe />));
    expect(running).toBe(false);
    delta({ projectId: 4, chatId: 40, event: { type: "toolCallStarted", name: "t", args: {} } });
    expect(running).toBe(true);
    delta({ projectId: 4, chatId: 40, event: { type: "done", answer: "", reason: "finish", reasoning: null } });
    expect(running).toBe(false);
    expect(ready).toBe(true);
  });
});

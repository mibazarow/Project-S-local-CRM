import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const bus = vi.hoisted(() => ({
  fs: undefined as ((p: { projectId: number; relPath: string }) => void) | undefined,
  tasks: undefined as ((p: { projectId: number }) => void) | undefined,
  agent: undefined as
    | ((p: { projectId?: number; chatId: number; event: Record<string, unknown> }) => void)
    | undefined,
}));

// Render counter for every MarkdownMessage mount/render: the mock is NOT
// memoized, so a settled row re-render always bumps its text's count. The
// settled agent answers are "a1"/"a2"; streamed text arrives under growing
// keys ("tok1", "tok1tok2", …).
const mdRenders = vi.hoisted(() => new Map<string, number>());

vi.mock("./MarkdownMessage", () => ({
  MarkdownMessage: ({ text }: { text: string }) => {
    mdRenders.set(text, (mdRenders.get(text) ?? 0) + 1);
    return <div className="bubble md">{text}</div>;
  },
}));

const history = vi.hoisted(() => [
  {
    id: 1,
    chatId: 10,
    role: "user",
    content: "q1",
    reasoning: null,
    toolCalls: [],
    toolCallId: null,
    name: null,
    createdAt: "2026-01-01T00:00:00Z",
    position: 0,
    attachments: [],
    pinned: false,
  },
  {
    id: 2,
    chatId: 10,
    role: "assistant",
    content: "a1",
    reasoning: null,
    toolCalls: [],
    toolCallId: null,
    name: null,
    createdAt: "2026-01-01T00:00:00Z",
    position: 1,
    attachments: [],
    pinned: false,
  },
  {
    id: 3,
    chatId: 10,
    role: "user",
    content: "q2",
    reasoning: null,
    toolCalls: [],
    toolCallId: null,
    name: null,
    createdAt: "2026-01-01T00:00:00Z",
    position: 2,
    attachments: [],
    pinned: false,
  },
  {
    id: 4,
    chatId: 10,
    role: "assistant",
    content: "a2",
    reasoning: null,
    toolCalls: [],
    toolCallId: null,
    name: null,
    createdAt: "2026-01-01T00:00:00Z",
    position: 3,
    attachments: [],
    pinned: false,
  },
]);

const projects = vi.hoisted(() => [
  {
    id: 1,
    name: "example",
    path: "/p/example",
    createdAt: "2026-01-01T00:00:00Z",
    openedAt: null,
    position: 0,
    fileCount: 1,
    taskCount: 0,
    overdueCount: 0,
    hasActiveTasks: false,
    taskColumns: ["a"],
    filesView: "list",
    archived: false,
    archivedAt: null,
    builtin: false,
  },
]);

vi.mock("../../api/ipc", () => ({
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws-perf" })),
  listProjects: vi.fn(async () => projects),
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
    void cb;
    return () => {};
  }),
  onProjectsChanged: vi.fn(async (cb: () => void) => {
    void cb;
    return () => {};
  }),
  onScheduleOpen: vi.fn(async (cb: (p: { sessionId: number }) => void) => {
    void cb;
    return () => {};
  }),
  projectChatHistory: vi.fn(async () => history),
  sendProjectMessage: vi.fn(async () => 10),
  getVisionStatus: vi.fn(async () => ({ supported: true })),
  onConfigChanged: vi.fn(async () => () => {}),
  todosList: vi.fn(async () => []),
  onTodosChanged: vi.fn(async () => () => {}),
  onAgentEvent: vi.fn(
    async (
      cb: (p: { projectId?: number; chatId: number; event: Record<string, unknown> }) => void,
    ) => {
      bus.agent = cb;
      return () => {
        bus.agent = undefined;
      };
    },
  ),
  onAgentConfirmation: vi.fn(async () => () => {}),
  onAgentFile: vi.fn(async () => () => {}),
  onAgentCompaction: vi.fn(async () => () => {}),
  onSkillProposal: vi.fn(async () => () => {}),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ onDragDropEvent: async () => () => {} }),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

import { AppProvider } from "../../state/AppStore";
import { resetChatRunStore } from "../../state/chatRunStore";
import ProjectChat from "./ProjectChat";

const delta = (text: string) =>
  act(() => {
    bus.agent?.({ projectId: 1, chatId: 10, event: { type: "textDelta", text } });
  });

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  mdRenders.clear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  resetChatRunStore();
});

describe("ProjectChat re-render isolation", () => {
  it("streaming tokens re-render only the streaming bubble, not settled rows", async () => {
    await act(async () => {
      root.render(
        <AppProvider>
          <ProjectChat projectId={1} projectName="example" />
        </AppProvider>,
      );
    });
    expect(mdRenders.get("a1")).toBeGreaterThan(0);
    expect(mdRenders.get("a2")).toBeGreaterThan(0);

    delta("tok1");
    const settledA1 = mdRenders.get("a1");
    const settledA2 = mdRenders.get("a2");
    expect(mdRenders.get("tok1")).toBe(1);

    delta("tok2");
    delta("tok3");
    // The reveal keeps the streaming bubble current: the latest accumulated
    // text is on screen, while settled rows never re-render on tokens. The
    // exact number of intermediate streaming frames is reveal-dependent, so we
    // assert the bubble tracks the newest text rather than a fixed per-frame
    // count.
    expect(mdRenders.get("tok1tok2tok3")).toBeGreaterThan(0);
    expect(mdRenders.get("a1")).toBe(settledA1);
    expect(mdRenders.get("a2")).toBe(settledA2);
  });

  it("tasks/fs data bumps do not re-render settled message rows", async () => {
    await act(async () => {
      root.render(
        <AppProvider>
          <ProjectChat projectId={1} projectName="example" />
        </AppProvider>,
      );
    });
    delta("tok1");
    const settledA1 = mdRenders.get("a1");
    const settledA2 = mdRenders.get("a2");

    act(() => bus.tasks?.({ projectId: 1 }));
    await act(async () => {});
    act(() => bus.fs?.({ projectId: 1, relPath: "x.txt" }));
    await act(async () => {});

    expect(mdRenders.get("a1")).toBe(settledA1);
    expect(mdRenders.get("a2")).toBe(settledA2);
  });
});

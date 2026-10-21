import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

// Captures the webview drag-drop callback so tests can fire Finder (OS)
// enter/over/drop events exactly as useChatAttachments receives them in Tauri.
const dropBus = vi.hoisted(() => ({
  cb: null as null | ((event: { payload: Record<string, unknown> }) => void),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => null) }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: vi.fn(
      async (cb: (event: { payload: Record<string, unknown> }) => void) => {
        dropBus.cb = cb;
        return () => {
          dropBus.cb = null;
        };
      },
    ),
  }),
}));

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

const sessions = vi.hoisted(() => [
  {
    id: 1,
    title: null,
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    lastActiveAt: "2026-01-01T00:00:00Z",
    pinned: false,
    pinnedAt: null,
  },
]);

vi.mock("../../api/ipc", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/ipc")>()),
  onboardingStatus: vi.fn(async () => ({ configured: true, workingDir: "/ws" })),
  listProjects: vi.fn(async () => projects),
  touchProjectOpened: vi.fn(async () => undefined),
  onFsChanged: vi.fn(async () => () => {}),
  onTasksChanged: vi.fn(async () => () => {}),
  onSchedulesChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
  onScheduleOpen: vi.fn(async () => () => {}),
  onSessionsChanged: vi.fn(async () => () => {}),
  onConfigChanged: vi.fn(async () => () => {}),
  onAgentEvent: vi.fn(async () => () => {}),
  onAgentConfirmation: vi.fn(async () => () => {}),
  onAgentFile: vi.fn(async () => () => {}),
  onAgentCompaction: vi.fn(async () => () => {}),
  onAgentCompactionFailed: vi.fn(async () => () => {}),
  onSkillProposal: vi.fn(async () => () => {}),
  onTodosChanged: vi.fn(async () => () => {}),
  todosList: vi.fn(async () => []),
  getVisionStatus: vi.fn(async () => ({ supported: true })),
  projectChatHistory: vi.fn(async () => []),
  sendProjectMessage: vi.fn(async () => 10),
  ensureNotesChat: vi.fn(async () => 7),
  notesChatHistory: vi.fn(async () => []),
  sendNotesMessage: vi.fn(async () => 7),
  harnessListSessions: vi.fn(async () => sessions),
  harnessSessionHistory: vi.fn(async () => []),
  harnessCreateSession: vi.fn(async () => sessions[0]),
  sendSessionMessage: vi.fn(async () => 11),
  indexChatMessages: vi.fn(async () => 0),
  scheduleList: vi.fn(async () => []),
  attachmentDataUrl: vi.fn(async () => "data:image/png;base64,AA"),
  attachFiles: vi.fn(async (_projectId: number | null, _sessionId: number | null, paths: string[]) =>
    paths.map((p, i) => ({
      name: p.slice(p.lastIndexOf("/") + 1),
      savedPath: `/inbox/${i}-${p.slice(p.lastIndexOf("/") + 1)}`,
      kind: "document" as const,
      size: 1,
      isImage: false,
      isText: true,
    })),
  ),
}));

import { AppProvider } from "../../state/AppStore";
import { resetChatRunStore } from "../../state/chatRunStore";
import * as ipc from "../../api/ipc";
import ProjectChat from "./ProjectChat";
import NotesChat from "../notes/NotesChat";
import HarnessView from "../harness/HarnessView";

let container: HTMLDivElement;
let root: Root;
// Element reported by the mocked document.elementFromPoint during an OS drag.
let hoverEl: Element | null;

beforeEach(() => {
  hoverEl = null;
  vi.mocked(ipc.attachFiles).mockClear();
  vi.mocked(ipc.sendProjectMessage).mockClear();
  vi.mocked(ipc.sendNotesMessage).mockClear();
  vi.mocked(ipc.sendSessionMessage).mockClear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  document.elementFromPoint = vi.fn(() => hoverEl) as unknown as typeof document.elementFromPoint;
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  resetChatRunStore();
});

const fire = (type: string, paths: string[] = []) =>
  act(() => {
    dropBus.cb?.({ payload: { type, position: { x: 5, y: 5 }, paths } });
  });

const zone = () => container.querySelector("[data-chat-drop]");
const chips = () => container.querySelectorAll(".attach-chip");

const mount = async (node: ReactNode) => {
  await act(async () => {
    root.render(<AppProvider>{node}</AppProvider>);
  });
};

const typeText = (value: string) => {
  const ta = container.querySelector<HTMLTextAreaElement>(".chat-input textarea")!;
  const setter = Object.getOwnPropertyDescriptor(
    window.HTMLTextAreaElement.prototype,
    "value",
  )!.set!;
  act(() => {
    setter.call(ta, value);
    ta.dispatchEvent(new Event("input", { bubbles: true }));
  });
};

const clickSend = async () => {
  await act(async () => {
    container.querySelector<HTMLButtonElement>(".send-btn")!.click();
  });
};

describe("Project chat Finder drop", () => {
  beforeEach(async () => {
    await mount(<ProjectChat projectId={1} projectName="example" />);
  });

  it("drop over the chat attaches several files and clears the highlight", async () => {
    hoverEl = zone();
    fire("enter");
    expect(zone()!.className).toContain("drag-over");
    fire("drop", ["/tmp/счёт.pdf", "/tmp/фото.png"]);
    expect(chips()).toHaveLength(2);
    expect(zone()!.className).not.toContain("drag-over");
  });

  it("dropped paths are attached through attachFiles on send", async () => {
    hoverEl = zone();
    fire("drop", ["/tmp/счёт.pdf", "/tmp/фото.png"]);
    typeText("проверь счёт");
    await clickSend();
    expect(ipc.attachFiles).toHaveBeenCalledWith(1, null, ["/tmp/счёт.pdf", "/tmp/фото.png"]);
    const refs = vi.mocked(ipc.sendProjectMessage).mock.calls[0]?.[2];
    expect(refs).toHaveLength(2);
    expect(refs?.[0].savedPath).toContain("счёт.pdf");
  });

  it("text and URL payloads are ignored without chips or errors", async () => {
    hoverEl = zone();
    fire("drop", ["просто текст", "https://example.ru/a.png", "rel/path.txt"]);
    expect(chips()).toHaveLength(0);
    expect(ipc.attachFiles).not.toHaveBeenCalled();
  });

  it("drop outside the chat zone is ignored", async () => {
    const outside = document.createElement("div");
    container.appendChild(outside);
    hoverEl = outside;
    fire("over");
    expect(zone()!.className).not.toContain("drag-over");
    fire("drop", ["/tmp/a.txt"]);
    expect(chips()).toHaveLength(0);
  });

  it("leave cancels the highlight", async () => {
    hoverEl = zone();
    fire("over");
    expect(zone()!.className).toContain("drag-over");
    fire("leave");
    expect(zone()!.className).not.toContain("drag-over");
  });
});

it("project chat without a selected project ignores drops", async () => {
  await mount(<ProjectChat projectId={null} projectName={null} />);
  hoverEl = zone();
  fire("over");
  expect(zone()!.className).not.toContain("drag-over");
  fire("drop", ["/tmp/a.txt"]);
  expect(chips()).toHaveLength(0);
});

describe("Harness chat Finder drop", () => {
  beforeEach(async () => {
    await mount(<HarnessView />);
  });

  it("drop over the chat attaches files and send routes them to the session", async () => {
    hoverEl = zone();
    fire("drop", ["/tmp/отчёт.docx"]);
    expect(chips()).toHaveLength(1);
    typeText("разбери отчёт");
    await clickSend();
    expect(ipc.attachFiles).toHaveBeenCalledWith(null, 1, ["/tmp/отчёт.docx"]);
    const refs = vi.mocked(ipc.sendSessionMessage).mock.calls[0]?.[2];
    expect(refs).toHaveLength(1);
  });

  it("text payloads are ignored", async () => {
    hoverEl = zone();
    fire("drop", ["ссылка: https://example.ru"]);
    expect(chips()).toHaveLength(0);
  });
});

describe("Notes chat Finder drop", () => {
  beforeEach(async () => {
    await mount(<NotesChat />);
  });

  it("drop over the chat attaches files and send routes them to the notes chat", async () => {
    hoverEl = zone();
    fire("enter");
    expect(zone()!.className).toContain("drag-over");
    fire("drop", ["/tmp/договор.pdf", "/tmp/скан.png"]);
    expect(chips()).toHaveLength(2);
    expect(zone()!.className).not.toContain("drag-over");
    typeText("что в договоре");
    await clickSend();
    expect(ipc.attachFiles).toHaveBeenCalledWith(null, null, ["/tmp/договор.pdf", "/tmp/скан.png"], true);
    const refs = vi.mocked(ipc.sendNotesMessage).mock.calls[0]?.[1];
    expect(refs).toHaveLength(2);
  });

  it("folders and text are ignored silently", async () => {
    hoverEl = zone();
    fire("drop", ["папка", "http://example.ru"]);
    expect(chips()).toHaveLength(0);
  });
});

it("no attach when no chat is mounted", async () => {
  await mount(<NotesChat />);
  act(() => root.unmount());
  expect(dropBus.cb).toBeNull();
  dropBus.cb?.({ payload: { type: "drop", position: { x: 5, y: 5 }, paths: ["/tmp/a.txt"] } });
  expect(chips()).toHaveLength(0);
  root = createRoot(container);
});

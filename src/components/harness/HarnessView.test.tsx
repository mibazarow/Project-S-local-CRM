import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ onDragDropEvent: vi.fn(async () => () => {}) }),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => null) }));

vi.mock("../../api/ipc", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/ipc")>()),
  harnessListSessions: vi.fn(async () => []),
  scheduleList: vi.fn(async () => []),
  scheduleOpen: vi.fn(async () => ({ sessionId: 1 })),
  scheduleDelete: vi.fn(async () => undefined),
  harnessDeleteSession: vi.fn(async () => undefined),
  harnessPinSession: vi.fn(async () => []),
  harnessSessionHistory: vi.fn(async () => []),
  harnessCreateSession: vi.fn(async () => ({ id: 1, title: null, lastActiveAt: 0 })),
  sendSessionMessage: vi.fn(async () => 1),
  indexChatMessages: vi.fn(async () => 0),
  getVisionStatus: vi.fn(async () => ({ supported: true })),
  onSessionsChanged: vi.fn(async () => () => {}),
  onConfigChanged: vi.fn(async () => () => {}),
  onAgentEvent: vi.fn(async () => () => {}),
  onAgentConfirmation: vi.fn(async () => () => {}),
  onAgentFile: vi.fn(async () => () => {}),
  onAgentCompaction: vi.fn(async () => () => {}),
  onSkillProposal: vi.fn(async () => () => {}),
}));

vi.mock("../../state/AppStore", () => ({
  useApp: () => ({
    state: { schedulesVersion: 0, pendingSessionId: null },
    actions: { clearPendingSession: vi.fn(), requestSession: vi.fn() },
  }),
}));

vi.mock("../chat/ChatComposer", () => ({ ChatComposer: () => null }));
vi.mock("../chat/TodoPanel", () => ({ default: () => null }));

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import * as ipc from "../../api/ipc";
import { registerConfirmHost, type ConfirmOptions } from "../../shared/confirm";
import HarnessView from "./HarnessView";

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
    root.render(<HarnessView />);
  });
}

describe("HarnessView new-chat button (Q-21)", () => {
  it("renders a blue labeled button with a white plus and the text «Новый чат»", async () => {
    await mount();
    const btn = container.querySelector<HTMLButtonElement>(".new-chat-btn");
    expect(btn).not.toBeNull();
    expect(btn!.getAttribute("title")).toBe("Новая сессия");
    expect(btn!.textContent).toContain("Новый чат");
    expect(btn!.querySelector("svg")).not.toBeNull();
    // The plus icon is decorative; the visible label carries the accessible name.
    expect(btn!.querySelector("svg")!.getAttribute("aria-hidden")).toBe("true");
  });

  it("clicking the button creates a session through the unchanged handler", async () => {
    await mount();
    expect(ipc.harnessCreateSession).not.toHaveBeenCalled();
    const btn = container.querySelector<HTMLButtonElement>(".new-chat-btn")!;
    await act(async () => {
      btn.click();
    });
    expect(ipc.harnessCreateSession).toHaveBeenCalledTimes(1);
  });
});

describe("HarnessView session pinning (Q.md pin slice)", () => {
  const sessions = [
    { id: 1, title: "First", createdAt: "", updatedAt: "", lastActiveAt: "2026-01-01", pinned: true, pinnedAt: "2026-02-01" },
    { id: 2, title: "Second", createdAt: "", updatedAt: "", lastActiveAt: "2026-01-02", pinned: false, pinnedAt: null },
  ];

  beforeEach(() => {
    vi.mocked(ipc.harnessListSessions).mockResolvedValue(sessions as never);
  });

  it("marks the pinned row's pin with the pinned class and leaves the unpinned one plain", async () => {
    await mount();
    const rows = container.querySelectorAll<HTMLElement>(".session-item");
    expect(rows).toHaveLength(2);
    const pinnedPin = rows[0].querySelector<HTMLButtonElement>(".session-pin")!;
    const plainPin = rows[1].querySelector<HTMLButtonElement>(".session-pin")!;
    expect(pinnedPin.classList.contains("pinned")).toBe(true);
    expect(pinnedPin.getAttribute("aria-pressed")).toBe("true");
    expect(plainPin.classList.contains("pinned")).toBe(false);
    expect(plainPin.getAttribute("aria-pressed")).toBe("false");
    expect(pinnedPin.querySelector("svg")).not.toBeNull();
  });

  it("toggles the pin through IPC and does not open/select the chat", async () => {
    await mount();
    const rows = container.querySelectorAll<HTMLElement>(".session-item");
    // The first session is active after load; the second is not.
    expect(rows[0].classList.contains("active")).toBe(true);
    expect(rows[1].classList.contains("active")).toBe(false);

    const plainPin = rows[1].querySelector<HTMLButtonElement>(".session-pin")!;
    await act(async () => {
      plainPin.click();
    });
    expect(ipc.harnessPinSession).toHaveBeenCalledTimes(1);
    expect(ipc.harnessPinSession).toHaveBeenCalledWith(2, true);
    // Clicking the pin must not select the chat: the second row stays inactive.
    const after = container.querySelectorAll<HTMLElement>(".session-item");
    expect(after[1].classList.contains("active")).toBe(false);
    expect(after[0].classList.contains("active")).toBe(true);
  });
});

describe("HarnessView schedules «+» opens a greeting chat (Q.md schedules slice)", () => {
  const GREETING =
    "Помогу создать расписание. Что нужно делать, в какое время и как часто — разово или регулярно?";

  it("creates a session with the static greeting, opens it, and makes no LLM call", async () => {
    const created = {
      id: 7,
      title: "Новое расписание",
      createdAt: "",
      updatedAt: "",
      lastActiveAt: "2026-01-01",
      pinned: false,
      pinnedAt: null,
    };
    vi.mocked(ipc.harnessCreateSession).mockResolvedValue(created as never);
    vi.mocked(ipc.harnessListSessions).mockResolvedValue([created] as never);
    await mount();

    // The old creation form is gone: no form markup anywhere in the panel.
    expect(container.querySelector(".schedule-form")).toBeNull();

    const btn = container.querySelector<HTMLButtonElement>(".schedule-add-btn")!;
    expect(btn.getAttribute("title")).toBe("Создать расписание в чате");
    await act(async () => {
      btn.click();
    });

    expect(ipc.harnessCreateSession).toHaveBeenCalledTimes(1);
    expect(ipc.harnessCreateSession).toHaveBeenCalledWith("Новое расписание", GREETING);
    // The greeting is seeded by the backend: no message is sent, no run starts.
    expect(ipc.sendSessionMessage).not.toHaveBeenCalled();

    // The new chat opens: its row is the active session.
    const rows = container.querySelectorAll<HTMLElement>(".session-item");
    expect(rows).toHaveLength(1);
    expect(rows[0].classList.contains("active")).toBe(true);
    expect(rows[0].textContent).toContain("Новое расписание");
  });
});

describe("HarnessView openSchedule loads history (Q.md item 3)", () => {
  const schedule = {
    id: 5,
    sessionId: 9,
    kind: "cron",
    spec: "0 9 * * *",
    prompt: null,
    status: "active",
    relatedTaskId: null,
    createdAt: "",
    lastRunAt: null,
    nextRunAt: null,
    timezone: null,
    error: null,
    title: "Ежедневный отчёт",
  };
  const session = {
    id: 9,
    title: "Расписание: Ежедневный отчёт",
    createdAt: "",
    updatedAt: "",
    lastActiveAt: "2026-01-01",
    pinned: false,
    pinnedAt: null,
  };

  beforeEach(() => {
    vi.mocked(ipc.scheduleList).mockResolvedValue([schedule] as never);
    vi.mocked(ipc.scheduleOpen).mockResolvedValue({ sessionId: 9 } as never);
    vi.mocked(ipc.harnessListSessions).mockResolvedValue([session] as never);
  });

  it("shows the existing history and never sends the auto-question", async () => {
    vi.mocked(ipc.harnessSessionHistory).mockResolvedValue([
      {
        id: 1,
        chatId: 77,
        role: "user",
        content: "старое сообщение из истории",
        reasoning: null,
        toolCalls: [],
        toolCallId: null,
        name: null,
        createdAt: "",
        position: 0,
        attachments: [],
        pinned: false,
      },
    ] as never);
    await mount();

    const item = container.querySelector<HTMLElement>(".schedule-item")!;
    await act(async () => {
      item.click();
    });

    expect(ipc.scheduleOpen).toHaveBeenCalledWith(5);
    expect(ipc.sendSessionMessage).not.toHaveBeenCalled();
    expect(container.textContent).toContain("старое сообщение из истории");
  });

  it("sends the auto-question only for a genuinely empty session", async () => {
    vi.mocked(ipc.harnessSessionHistory).mockResolvedValue([] as never);
    await mount();

    const item = container.querySelector<HTMLElement>(".schedule-item")!;
    await act(async () => {
      item.click();
    });

    expect(ipc.sendSessionMessage).toHaveBeenCalledTimes(1);
    const [sid, text] = vi.mocked(ipc.sendSessionMessage).mock.calls[0];
    expect(sid).toBe(9);
    expect(text).toContain("что это за задача по расписанию");
  });
});

describe("HarnessView cascade delete confirmations (Q.md item 4)", () => {
  let captured: ConfirmOptions | null;
  let resolveWith: boolean;

  beforeEach(() => {
    captured = null;
    resolveWith = true;
    registerConfirmHost((req) => {
      captured = req.opts;
      req.resolve(resolveWith);
    });
  });

  afterEach(() => {
    registerConfirmHost(null);
  });

  it("deleting a chat with linked schedules warns about both and cascades", async () => {
    vi.mocked(ipc.harnessListSessions).mockResolvedValue([
      { id: 1, title: "Чат с расписаниями", createdAt: "", updatedAt: "", lastActiveAt: "2026-01-01", pinned: false, pinnedAt: null },
    ] as never);
    vi.mocked(ipc.scheduleList).mockResolvedValue([
      { id: 10, sessionId: 1, kind: "cron", spec: "0 9 * * *", prompt: null, status: "active", relatedTaskId: null, createdAt: "", lastRunAt: null, nextRunAt: null, timezone: null, error: null, title: "A" },
      { id: 11, sessionId: 1, kind: "run_at", spec: "2026-11-01T10:00:00", prompt: null, status: "active", relatedTaskId: null, createdAt: "", lastRunAt: null, nextRunAt: null, timezone: null, error: null, title: "B" },
    ] as never);
    await mount();

    const del = container.querySelector<HTMLElement>(".session-item .mini-btn.danger")!;
    await act(async () => {
      del.click();
    });

    expect(captured!.message).toBe("Чат и 2 расписания будут удалены.");
    expect(ipc.harnessDeleteSession).toHaveBeenCalledWith(1);
  });

  it("deleting a schedule with a linked chat warns the chat goes too", async () => {
    vi.mocked(ipc.harnessListSessions).mockResolvedValue([] as never);
    vi.mocked(ipc.scheduleList).mockResolvedValue([
      { id: 12, sessionId: 9, kind: "cron", spec: "0 9 * * *", prompt: null, status: "active", relatedTaskId: null, createdAt: "", lastRunAt: null, nextRunAt: null, timezone: null, error: null, title: "Ежедневный отчёт" },
    ] as never);
    await mount();

    const del = container.querySelector<HTMLElement>(".schedule-item .mini-btn.danger")!;
    await act(async () => {
      del.click();
    });

    expect(captured!.message).toBe("Расписание «Ежедневный отчёт» и его чат будут удалены.");
    expect(ipc.scheduleDelete).toHaveBeenCalledWith(12);
  });

  it("cancel deletes nothing", async () => {
    resolveWith = false;
    vi.mocked(ipc.harnessListSessions).mockResolvedValue([
      { id: 1, title: "Чат", createdAt: "", updatedAt: "", lastActiveAt: "2026-01-01", pinned: false, pinnedAt: null },
    ] as never);
    vi.mocked(ipc.scheduleList).mockResolvedValue([
      { id: 10, sessionId: 1, kind: "cron", spec: "0 9 * * *", prompt: null, status: "active", relatedTaskId: null, createdAt: "", lastRunAt: null, nextRunAt: null, timezone: null, error: null, title: "A" },
    ] as never);
    await mount();

    const del = container.querySelector<HTMLElement>(".session-item .mini-btn.danger")!;
    await act(async () => {
      del.click();
    });

    expect(ipc.harnessDeleteSession).not.toHaveBeenCalled();
    expect(ipc.scheduleDelete).not.toHaveBeenCalled();
  });
});

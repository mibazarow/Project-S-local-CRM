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

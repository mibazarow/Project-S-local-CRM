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
  harnessSessionHistory: vi.fn(async () => []),
  harnessCreateSession: vi.fn(async () => ({ id: 1, title: null, lastActiveAt: 0 })),
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
vi.mock("./SchedulePanel", () => ({ default: () => null }));

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

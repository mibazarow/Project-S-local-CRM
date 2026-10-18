import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));

import SettingsModal from "./SettingsModal";

// React 19 refuses to run act() unless the act environment flag is set.
(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const called = (cmd: string) => invokeMock.mock.calls.filter((c) => c[0] === cmd).length;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string, args?: unknown) => {
    const kind = (args as { kind?: string } | undefined)?.kind;
    switch (cmd) {
      case "read_config":
        if (kind === "llm")
          return {
            baseUrl: "",
            apiKey: "",
            model: "",
            contextWindow: 100000,
            maxTokens: null,
            vision: null,
            thinkingLevel: "medium",
          };
        if (kind === "indexing")
          return { maxFileSizeBytes: 10485760, maxTextChars: 1000000, excludeDirs: [] };
        return {};
      case "skills_list":
      case "mcp_status":
      case "trash_list":
      case "list_projects":
        return [];
      case "backup_status":
        return { lastBackupAt: null, count: 0, intervalDays: 1, due: false };
      case "embed_status":
        return { available: false, modelPresent: false, reason: null };
      case "get_vision_status":
        return { supported: null, overridden: false, reason: null };
      case "config_dir_path":
      case "backups_dir":
        return "";
      case "get_working_dir":
        return null;
      case "get_autostart":
        return false;
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
    root.render(<SettingsModal onClose={() => {}} />);
  });
}

const tabButton = (label: string) =>
  Array.from(container.querySelectorAll<HTMLButtonElement>(".settings-tab")).find(
    (b) => b.textContent === label,
  )!;

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

describe("SettingsModal lazy tabs", () => {
  it("does not load skills/MCP/trash/projects/embed data on modal mount", async () => {
    await mount();
    // The default (Модель) tab and the shared app.json writer read their config.
    expect(called("read_config")).toBeGreaterThanOrEqual(2);
    expect(called("skills_list")).toBe(0);
    expect(called("mcp_status")).toBe(0);
    expect(called("trash_list")).toBe(0);
    expect(called("list_projects")).toBe(0);
    expect(called("embed_status")).toBe(0);
    expect(called("backup_status")).toBe(0);
    expect(called("get_working_dir")).toBe(0);
  });

  it("loads a tab's data on first open and keeps it mounted across switches", async () => {
    await mount();
    await click(tabButton("Скиллы"));
    expect(called("skills_list")).toBe(1);
    await click(tabButton("Модель"));
    await click(tabButton("Скиллы"));
    // Stays mounted once opened: switching back does not re-read the list.
    expect(called("skills_list")).toBe(1);
    // Only one settings-content is in the DOM at a time.
    expect(container.querySelectorAll(".settings-content")).toHaveLength(1);
  });

  it("loads Прочее data only when the tab is first opened", async () => {
    await mount();
    await click(tabButton("Прочее"));
    for (const cmd of [
      "trash_list",
      "list_projects",
      "backup_status",
      "backups_dir",
      "get_autostart",
      "get_working_dir",
      "config_dir_path",
    ]) {
      expect(called(cmd), cmd).toBe(1);
    }
  });
});

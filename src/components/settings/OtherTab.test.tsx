import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DebouncedConfig } from "./useDebouncedConfig";
import type { AppForm } from "./forms";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const setZoomMock = vi.hoisted(() => vi.fn(async (_z: number) => {}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(), save: vi.fn() }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ setZoom: (z: number) => setZoomMock(z) }),
}));
vi.mock("../../api/ipc", () => ({
  getWorkingDir: vi.fn(async () => "/ws"),
  configDirPath: vi.fn(async () => "/cfg"),
  backupsDir: vi.fn(async () => "/bk"),
  getAutostart: vi.fn(async () => false),
  backupStatus: vi.fn(async () => null),
  trashList: vi.fn(async () => []),
  listProjects: vi.fn(async () => []),
  onConfigChanged: vi.fn(async () => () => {}),
  onTrashChanged: vi.fn(async () => () => {}),
  onProjectsChanged: vi.fn(async () => () => {}),
}));

import OtherTab from "./OtherTab";
import { applyUiScale } from "../../shared/uiScale";

let updateForm: (patch: Partial<AppForm>) => void;
let appStub: DebouncedConfig<AppForm>;

function makeApp(uiScalePercent: number): DebouncedConfig<AppForm> {
  return {
    form: {
      autopilot: false,
      sessionTtlDays: "30",
      tmpRetentionDays: "7",
      logRetentionDays: "14",
      backupIntervalDays: "1",
      backupKeep: "7",
      uiScalePercent,
    },
    updateForm,
    reload: () => {},
    flush: async () => {},
    focusField: () => () => {},
    blurField: () => {},
    focusedRef: { current: null },
  };
}

let container: HTMLDivElement;
let root: Root;

beforeEach(async () => {
  setZoomMock.mockReset();
  setZoomMock.mockImplementation(async () => {});
  await applyUiScale(100);
  setZoomMock.mockClear();
  updateForm = vi.fn((patch: Partial<AppForm>) => {
    if (appStub.form) Object.assign(appStub.form, patch);
  });
  appStub = makeApp(100);
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function mount(app: DebouncedConfig<AppForm>) {
  await act(async () => {
    root.render(<OtherTab active error={null} setError={() => {}} app={app} />);
  });
  await act(async () => {});
}

const buttons = () => Array.from(container.querySelectorAll<HTMLButtonElement>("button"));
const btnByText = (label: string) =>
  buttons().find((b) => b.textContent?.trim() === label)!;

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

describe("OtherTab UI scale control", () => {
  it("renders the four discrete steps and the current value label", async () => {
    appStub = makeApp(130);
    await mount(appStub);
    for (const step of ["100", "110", "130", "175"]) {
      expect(btnByText(step)).toBeTruthy();
    }
    for (const removed of ["125", "150", "200"]) {
      expect(btnByText(removed)).toBeUndefined();
    }
    expect(container.textContent).toContain("130 %");
  });

  it("clicking a step persists it and applies the zoom live", async () => {
    await mount(appStub);
    await click(btnByText("130"));
    expect(updateForm).toHaveBeenCalledWith({ uiScalePercent: 130 });
    expect(setZoomMock).toHaveBeenCalledWith(1.3);
  });

  it("shows the zoom failure and retries it", async () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    appStub = makeApp(110);
    await mount(appStub);
    setZoomMock.mockRejectedValueOnce(new Error("denied by ACL"));

    await click(btnByText("175"));
    expect(updateForm).toHaveBeenCalledWith({ uiScalePercent: 175 });
    expect(container.textContent).toContain("Не удалось применить масштаб");
    expect(container.textContent).toContain("denied by ACL");

    setZoomMock.mockImplementation(async () => {});
    await click(btnByText("Повторить"));
    expect(setZoomMock).toHaveBeenLastCalledWith(1.75);
    expect(container.textContent).not.toContain("Не удалось применить масштаб");
    spy.mockRestore();
  });

  it("reset returns the scale to 100 %", async () => {
    appStub = makeApp(175);
    await mount(appStub);
    const reset = btnByText("Сбросить");
    expect(reset.disabled).toBe(false);
    await click(reset);
    expect(updateForm).toHaveBeenCalledWith({ uiScalePercent: 100 });
    expect(setZoomMock).toHaveBeenCalledWith(1);
  });

  it("disables reset when already at 100 %", async () => {
    await mount(appStub);
    expect(btnByText("Сбросить").disabled).toBe(true);
  });
});

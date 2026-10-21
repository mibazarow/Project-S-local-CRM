import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const { readConfigMock, setZoomMock, configBus } = vi.hoisted(() => ({
  readConfigMock: vi.fn(),
  setZoomMock: vi.fn(async (_z: number) => {}),
  configBus: { cb: undefined as undefined | ((p: { which: string }) => void) },
}));

vi.mock("../../api/ipc", () => ({
  readConfig: (...args: unknown[]) => readConfigMock(...args),
  onConfigChanged: vi.fn(async (cb: (p: { which: string }) => void) => {
    configBus.cb = cb;
    return () => {
      configBus.cb = undefined;
    };
  }),
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ setZoom: (z: number) => setZoomMock(z) }),
}));

import UiScaleApplier from "./UiScaleApplier";

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  readConfigMock.mockReset();
  setZoomMock.mockReset();
  setZoomMock.mockImplementation(async () => {});
  configBus.cb = undefined;
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
    root.render(<UiScaleApplier />);
  });
  await act(async () => {});
}

describe("UiScaleApplier", () => {
  it("applies the saved zoom at startup", async () => {
    readConfigMock.mockResolvedValue({ uiScalePercent: 150 });
    await mount();
    expect(setZoomMock).toHaveBeenCalledWith(1.5);
  });

  it("normalizes an invalid saved scale to 100%", async () => {
    readConfigMock.mockResolvedValue({ uiScalePercent: 137 });
    await mount();
    expect(setZoomMock).toHaveBeenCalledWith(1);
  });

  it("re-applies when the app config changes", async () => {
    readConfigMock.mockResolvedValue({ uiScalePercent: 100 });
    await mount();
    expect(setZoomMock).toHaveBeenLastCalledWith(1);
    readConfigMock.mockResolvedValue({ uiScalePercent: 125 });
    await act(async () => {
      configBus.cb?.({ which: "app" });
    });
    await act(async () => {});
    expect(setZoomMock).toHaveBeenLastCalledWith(1.25);
  });

  it("ignores config changes for other kinds", async () => {
    readConfigMock.mockResolvedValue({ uiScalePercent: 100 });
    await mount();
    const calls = setZoomMock.mock.calls.length;
    await act(async () => {
      configBus.cb?.({ which: "llm" });
    });
    expect(setZoomMock.mock.calls.length).toBe(calls);
  });

  it("stays silent when the config cannot be read", async () => {
    readConfigMock.mockRejectedValue(new Error("no working dir"));
    await mount();
    expect(setZoomMock).not.toHaveBeenCalled();
  });

  it("registers no Cmd +/-/0 zoom hotkeys", async () => {
    readConfigMock.mockResolvedValue({ uiScalePercent: 100 });
    await mount();
    const calls = setZoomMock.mock.calls.length;
    for (const code of ["Equal", "Minus", "NumpadAdd", "Digit0", "Numpad0"]) {
      await act(async () => {
        window.dispatchEvent(
          new KeyboardEvent("keydown", { key: "+", code, metaKey: true, bubbles: true }),
        );
      });
    }
    expect(setZoomMock.mock.calls.length).toBe(calls);
  });
});

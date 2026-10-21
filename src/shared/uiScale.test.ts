import { beforeEach, describe, expect, it, vi } from "vitest";

const { setZoomMock, getWebviewMock } = vi.hoisted(() => ({
  setZoomMock: vi.fn(async () => {}),
  getWebviewMock: vi.fn(() => ({ setZoom: setZoomMock })),
}));

vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => getWebviewMock(),
}));

import {
  DEFAULT_UI_SCALE_PERCENT,
  UI_SCALE_STEPS,
  applyUiScale,
  getUiScaleError,
  normalizeUiScale,
  subscribeUiScaleError,
  uiScaleLabel,
} from "./uiScale";

describe("UI_SCALE_STEPS", () => {
  it("offers exactly 100, 110, 130 and 175", () => {
    expect(UI_SCALE_STEPS).toEqual([100, 110, 130, 175]);
  });
});

describe("normalizeUiScale", () => {
  it("keeps every shipped step", () => {
    for (const step of UI_SCALE_STEPS) expect(normalizeUiScale(step)).toBe(step);
  });

  it("keeps the previously saved 110", () => {
    expect(normalizeUiScale(110)).toBe(110);
  });

  it("maps anything outside the steps to the default 100", () => {
    for (const bad of [
      undefined,
      null,
      NaN,
      0,
      99,
      101,
      105,
      120,
      125,
      137,
      150,
      200,
      201,
      1000,
      "abc",
      "",
    ]) {
      expect(normalizeUiScale(bad)).toBe(DEFAULT_UI_SCALE_PERCENT);
    }
  });

  it("accepts a numeric string step", () => {
    expect(normalizeUiScale("130")).toBe(130);
  });
});

describe("uiScaleLabel", () => {
  it("formats the value with a percent sign", () => {
    expect(uiScaleLabel(130)).toBe("130 %");
    expect(uiScaleLabel(137)).toBe("100 %");
  });
});

describe("applyUiScale", () => {
  beforeEach(async () => {
    setZoomMock.mockReset();
    setZoomMock.mockImplementation(async () => {});
    getWebviewMock.mockReset();
    getWebviewMock.mockImplementation(() => ({ setZoom: setZoomMock }));
    await applyUiScale(100);
    setZoomMock.mockClear();
  });

  it("sets the webview zoom to the percent as a factor", async () => {
    await applyUiScale(130);
    expect(setZoomMock).toHaveBeenCalledWith(1.3);
    await applyUiScale(100);
    expect(setZoomMock).toHaveBeenLastCalledWith(1);
  });

  it("normalizes an invalid value before zooming", async () => {
    await applyUiScale(137);
    expect(setZoomMock).toHaveBeenCalledWith(1);
  });

  it("records and logs a failed zoom", async () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    const listener = vi.fn();
    const unsubscribe = subscribeUiScaleError(listener);
    setZoomMock.mockRejectedValueOnce(new Error("denied by ACL"));

    await expect(applyUiScale(175)).resolves.toBe(false);
    expect(getUiScaleError()).toBe("denied by ACL");
    expect(spy).toHaveBeenCalled();
    expect(listener).toHaveBeenCalled();

    unsubscribe();
    spy.mockRestore();
  });

  it("clears the error after a successful zoom", async () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    setZoomMock.mockRejectedValueOnce(new Error("zoom failed"));
    await applyUiScale(130);
    expect(getUiScaleError()).toBe("zoom failed");

    await applyUiScale(130);
    expect(getUiScaleError()).toBeNull();
    await expect(applyUiScale(130)).resolves.toBe(true);
    spy.mockRestore();
  });

  it("records an unavailable webview API as an error", async () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    getWebviewMock.mockImplementationOnce(() => {
      throw new Error("not running under Tauri");
    });
    await expect(applyUiScale(130)).resolves.toBe(false);
    expect(getUiScaleError()).toBe("not running under Tauri");
    expect(setZoomMock).not.toHaveBeenCalled();
    spy.mockRestore();
  });
});

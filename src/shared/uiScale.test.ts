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
  normalizeUiScale,
  uiScaleLabel,
} from "./uiScale";

describe("normalizeUiScale", () => {
  it("keeps every shipped step", () => {
    for (const step of UI_SCALE_STEPS) expect(normalizeUiScale(step)).toBe(step);
  });

  it("maps anything outside the steps to the default 100", () => {
    for (const bad of [undefined, null, NaN, 0, 99, 101, 120, 137, 201, 1000, "abc", ""]) {
      expect(normalizeUiScale(bad)).toBe(DEFAULT_UI_SCALE_PERCENT);
    }
  });

  it("accepts a numeric string step", () => {
    expect(normalizeUiScale("125")).toBe(125);
  });
});

describe("uiScaleLabel", () => {
  it("formats the value with a percent sign", () => {
    expect(uiScaleLabel(125)).toBe("125 %");
    expect(uiScaleLabel(137)).toBe("100 %");
  });
});

describe("applyUiScale", () => {
  beforeEach(() => {
    setZoomMock.mockReset();
    setZoomMock.mockImplementation(async () => {});
    getWebviewMock.mockReset();
    getWebviewMock.mockImplementation(() => ({ setZoom: setZoomMock }));
  });

  it("sets the webview zoom to the percent as a factor", async () => {
    await applyUiScale(150);
    expect(setZoomMock).toHaveBeenCalledWith(1.5);
    await applyUiScale(100);
    expect(setZoomMock).toHaveBeenLastCalledWith(1);
  });

  it("normalizes an invalid value before zooming", async () => {
    await applyUiScale(137);
    expect(setZoomMock).toHaveBeenCalledWith(1);
  });

  it("degrades gracefully when the webview API is unavailable", async () => {
    getWebviewMock.mockImplementationOnce(() => {
      throw new Error("not running under Tauri");
    });
    await expect(applyUiScale(150)).resolves.toBeUndefined();
    expect(setZoomMock).not.toHaveBeenCalled();
  });

  it("swallows a rejected setZoom", async () => {
    setZoomMock.mockRejectedValueOnce(new Error("zoom failed"));
    await expect(applyUiScale(125)).resolves.toBeUndefined();
  });
});

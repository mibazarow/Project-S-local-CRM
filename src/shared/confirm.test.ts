import { afterEach, describe, expect, it, vi } from "vitest";
import { confirmRemove } from "./confirm";

afterEach(() => {
  vi.restoreAllMocks();
});

describe("confirmRemove", () => {
  it("asks with the exact «Удалить …?» phrasing and forwards the answer", () => {
    const spy = vi.spyOn(window, "confirm").mockReturnValue(true);
    expect(confirmRemove('файл «notes.md»')).toBe(true);
    expect(spy).toHaveBeenCalledWith("Удалить файл «notes.md»?");
  });

  it("keeps the folder phrasing verbatim", () => {
    const spy = vi.spyOn(window, "confirm").mockReturnValue(false);
    expect(confirmRemove('папку «src» со всем содержимым')).toBe(false);
    expect(spy).toHaveBeenCalledWith("Удалить папку «src» со всем содержимым?");
  });
});

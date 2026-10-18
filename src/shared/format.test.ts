import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { dueMs, formatActivity, formatDue, formatInstant, formatModified } from "./format";

// Fix "now" to 2026-10-18 12:00 local so the «сегодня» branch is deterministic.
beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(2026, 9, 18, 12, 0, 0));
});

afterEach(() => {
  vi.useRealTimers();
});

describe("formatActivity", () => {
  it("renders today as «сегодня HH:MM»", () => {
    expect(formatActivity("2026-10-18 09:05:00")).toBe("сегодня 09:05");
  });
  it("renders other days as «D month HH:MM»", () => {
    expect(formatActivity("2026-10-05 14:30:00")).toBe("5 окт. 14:30");
    expect(formatActivity("2025-12-31 23:59:00")).toBe("31 дек. 23:59");
  });
  it("returns the raw value for unparseable input", () => {
    expect(formatActivity("not-a-date")).toBe("not-a-date");
  });
});

describe("formatInstant", () => {
  it("renders ISO instants as «D month, HH:MM»", () => {
    expect(formatInstant("2026-10-05T14:30:00")).toBe("5 окт., 14:30");
  });
  it("returns the raw value for unparseable input", () => {
    expect(formatInstant("garbage")).toBe("garbage");
  });
});

describe("formatDue", () => {
  it("renders YYYY-MM-DD as «D month»", () => {
    expect(formatDue("2026-10-05")).toBe("5 окт.");
  });
  it("returns the raw value for unparseable input", () => {
    expect(formatDue("bad")).toBe("bad");
  });
});

describe("dueMs", () => {
  const at0700 = new Date(2026, 9, 18, 7, 0, 0).getTime();

  it("overdue boundary honors the due time around 07:00", () => {
    expect(dueMs("2026-10-18", "06:59")).toBeLessThan(at0700);
    expect(dueMs("2026-10-18", "07:01")).toBeGreaterThan(at0700);
    expect(dueMs("2026-10-18", "07:00")).toBe(at0700);
  });
  it("empty due time falls back to 07:00", () => {
    expect(dueMs("2026-10-18", "")).toBe(at0700);
  });
  it("returns NaN for unparseable dates", () => {
    expect(Number.isNaN(dueMs("bad", "07:00"))).toBe(true);
  });
});

describe("formatModified", () => {
  it("renders unix seconds as «сегодня HH:MM» on the current day", () => {
    expect(formatModified(new Date(2026, 9, 18, 9, 5).getTime() / 1000)).toBe("сегодня 09:05");
  });
  it("renders other days as «D month HH:MM»", () => {
    expect(formatModified(new Date(2026, 9, 5, 14, 30).getTime() / 1000)).toBe("5 окт. 14:30");
  });
  it("returns empty string for out-of-range timestamps", () => {
    expect(formatModified(Number.NaN)).toBe("");
    expect(formatModified(8.64e16)).toBe("");
  });
});

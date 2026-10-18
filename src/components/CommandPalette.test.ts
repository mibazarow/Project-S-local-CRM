import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import { score } from "./CommandPalette";

describe("score", () => {
  it("prefix match scores 100, case-insensitive on the text", () => {
    expect(score("Projects", "proj")).toBe(100);
    expect(score("projects", "projects")).toBe(100);
    expect(score("", "")).toBe(100);
  });

  it("empty query matches as a prefix (100)", () => {
    expect(score("anything", "")).toBe(100);
  });

  it("substring match scores 60 minus the position", () => {
    expect(score("xproj", "proj")).toBe(59);
    expect(score("My Projects file", "proj")).toBe(57);
  });

  it("substring position is capped at 30", () => {
    expect(score(`${"x".repeat(40)}abc`, "abc")).toBe(30);
    expect(score("x".repeat(30) + "abc", "abc")).toBe(30);
  });

  it("fuzzy subsequence scores 30 minus the total gaps", () => {
    expect(score("p-a-r", "par")).toBe(28);
    expect(score("p_x_r_o_j", "proj")).toBe(25);
  });

  it("fuzzy gaps are capped at 25", () => {
    expect(score(`a${"x".repeat(30)}b`, "ab")).toBe(5);
  });

  it("returns null when the query is not a subsequence", () => {
    expect(score("abc", "z")).toBeNull();
    expect(score("abc", "ad")).toBeNull();
    expect(score("abc", "cb")).toBeNull();
  });

  it("the query is not case-folded: uppercase query misses lowercase text", () => {
    expect(score("projects", "PROJECTS")).toBeNull();
  });

  it("ordering: prefix beats substring, substring beats fuzzy", () => {
    const prefix = score("proj", "proj");
    const substring = score("xproj", "proj");
    const fuzzy = score("p-r-o-j", "proj");
    expect(prefix).toBe(100);
    expect(substring).toBe(59);
    expect(fuzzy).toBe(30 - 3);
    expect(prefix!).toBeGreaterThan(substring!);
    expect(substring!).toBeGreaterThan(fuzzy!);
  });
});

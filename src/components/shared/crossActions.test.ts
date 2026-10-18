import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import { noteNameFromText } from "./crossActions";

describe("noteNameFromText", () => {
  it("uses the first non-empty trimmed line", () => {
    expect(noteNameFromText("Hello World")).toBe("Hello World.md");
    expect(noteNameFromText("  \n  Second line  \nthird")).toBe("Second line.md");
    expect(noteNameFromText("\t\n real")).toBe("real.md");
  });

  it("splits on CRLF and LF", () => {
    expect(noteNameFromText("line1\r\nline2")).toBe("line1.md");
  });

  it("replaces path-illegal and control characters with spaces", () => {
    expect(noteNameFromText('a/b:c*d?e"f<g>h|i')).toBe("a b c d e f g h i.md");
    expect(noteNameFromText("a\\b")).toBe("a b.md");
    expect(noteNameFromText("a\u0000b\u001fc")).toBe("a b c.md");
  });

  it("collapses runs of whitespace and trims", () => {
    expect(noteNameFromText("a   b")).toBe("a b.md");
    expect(noteNameFromText("  padded  ")).toBe("padded.md");
    expect(noteNameFromText("a . ")).toBe("a.md");
  });

  it("strips trailing dots and spaces after the length cut", () => {
    expect(noteNameFromText("Title...")).toBe("Title.md");
    expect(noteNameFromText("... ...")).toBe("Заметка.md");
  });

  it("falls back to Заметка for empty, blank or dot-only input", () => {
    expect(noteNameFromText("")).toBe("Заметка.md");
    expect(noteNameFromText("\n\n \n")).toBe("Заметка.md");
    expect(noteNameFromText(".")).toBe("Заметка.md");
  });

  it("cuts the name at 80 characters", () => {
    expect(noteNameFromText("x".repeat(100))).toBe(`${"x".repeat(80)}.md`);
    expect(noteNameFromText(`${"x".repeat(80)}...`)).toBe(`${"x".repeat(80)}.md`);
  });

  it("keeps a .md extension (case-insensitive), otherwise appends .md", () => {
    expect(noteNameFromText("note.md")).toBe("note.md");
    expect(noteNameFromText("Note.MD")).toBe("Note.MD");
    expect(noteNameFromText("a.md.")).toBe("a.md");
    expect(noteNameFromText("x.md.txt")).toBe("x.md.txt.md");
  });
});

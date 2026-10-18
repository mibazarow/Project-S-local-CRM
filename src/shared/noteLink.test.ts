import { describe, expect, it, vi } from "vitest";
import { openNoteLink, resolveNoteLink, setNoteLinkHandler } from "./noteLink";

describe("resolveNoteLink", () => {
  it("treats explicit note:/notes: scheme as a note", () => {
    expect(resolveNoteLink("note:foo.md")).toBe("foo.md");
    expect(resolveNoteLink("notes:Входящие/2026-10-18.md")).toBe("Входящие/2026-10-18.md");
    expect(resolveNoteLink("note:")).toBeNull();
  });

  it("treats relative .md/.markdown as a note", () => {
    expect(resolveNoteLink("foo.md")).toBe("foo.md");
    expect(resolveNoteLink("./a/b.md")).toBe("a/b.md");
    expect(resolveNoteLink("note.markdown")).toBe("note.markdown");
  });

  it("treats an absolute in-vault path as a note (part after /Notes/)", () => {
    expect(resolveNoteLink("/wd/Notes/a/b.md")).toBe("a/b.md");
  });

  it("leaves http/https/mailto/tel and other schemes alone", () => {
    expect(resolveNoteLink("http://example.ru/a.md")).toBeNull();
    expect(resolveNoteLink("https://example.ru/a.md")).toBeNull();
    expect(resolveNoteLink("mailto:a@example.ru")).toBeNull();
    expect(resolveNoteLink("tel:+10000000000")).toBeNull();
    expect(resolveNoteLink("ftp://example.ru/a.md")).toBeNull();
  });

  it("leaves non-md, traversal, anchors, empty and out-of-vault absolutes alone", () => {
    expect(resolveNoteLink("doc.pdf")).toBeNull();
    expect(resolveNoteLink("image.png")).toBeNull();
    expect(resolveNoteLink("../secret.md")).toBeNull();
    expect(resolveNoteLink("a/../b.md")).toBeNull();
    expect(resolveNoteLink("#section")).toBeNull();
    expect(resolveNoteLink("")).toBeNull();
    expect(resolveNoteLink("/wd/Projects/example.ru/a.md")).toBeNull();
  });
});

describe("note link handler channel", () => {
  it("routes openNoteLink through the registered handler and tolerates none", () => {
    const handler = vi.fn();
    setNoteLinkHandler(handler);
    openNoteLink("a.md");
    expect(handler).toHaveBeenCalledWith("a.md");
    setNoteLinkHandler(null);
    expect(() => openNoteLink("a.md")).not.toThrow();
  });
});

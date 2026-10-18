import { describe, expect, it } from "vitest";
import { joinMarkdownBlocks, splitMarkdownBlocks } from "./markdown";

describe("splitMarkdownBlocks", () => {
  it("splits on blank lines and keeps single-line runs together", () => {
    expect(splitMarkdownBlocks("a\nb\n\nc")).toEqual(["a\nb", "c"]);
  });

  it("returns no empty blocks for empty or blank-only sources", () => {
    expect(splitMarkdownBlocks("")).toEqual([]);
    expect(splitMarkdownBlocks("   \n\t\n")).toEqual([]);
  });

  it("collapses runs of blank lines into a single boundary", () => {
    expect(splitMarkdownBlocks("a\n\n\n\nb")).toEqual(["a", "b"]);
  });

  it("ignores leading and trailing blank lines", () => {
    expect(splitMarkdownBlocks("\na\n")).toEqual(["a"]);
    expect(splitMarkdownBlocks("\n\na\n\nb\n\n")).toEqual(["a", "b"]);
  });

  it("keeps a fenced code block with blank lines inside as one block", () => {
    expect(splitMarkdownBlocks("```\na\n\nb\n```")).toEqual(["```\na\n\nb\n```"]);
  });

  it("starts a block at a fence that follows a blank line", () => {
    expect(splitMarkdownBlocks("para\n\n```\nfenced\n```")).toEqual(["para", "```\nfenced\n```"]);
  });

  it("leaves an unterminated fence as the trailing block", () => {
    expect(splitMarkdownBlocks("para\n\n```\nunclosed")).toEqual(["para", "```\nunclosed"]);
  });

  it("a fence only closes with the same marker char and at least the opening length", () => {
    expect(splitMarkdownBlocks("```\ncode\n~~~\nmore")).toEqual(["```\ncode\n~~~\nmore"]);
    expect(splitMarkdownBlocks("~~~\nx\n```")).toEqual(["~~~\nx\n```"]);
    expect(splitMarkdownBlocks("```\nx\n````")).toEqual(["```\nx\n````"]);
    expect(splitMarkdownBlocks("```\nx\n``")).toEqual(["```\nx\n``"]);
  });

  it("recognizes indented fences", () => {
    expect(splitMarkdownBlocks("  ```\ncode\n  ```")).toEqual(["  ```\ncode\n  ```"]);
  });

  it("keeps tables, lists and blockquotes without blank lines in one block", () => {
    expect(splitMarkdownBlocks("| a | b |\n| - | - |\n| 1 | 2 |")).toEqual([
      "| a | b |\n| - | - |\n| 1 | 2 |",
    ]);
    expect(splitMarkdownBlocks("- a\n- b\n- c")).toEqual(["- a\n- b\n- c"]);
    expect(splitMarkdownBlocks("> q1\n> q2")).toEqual(["> q1\n> q2"]);
  });

  it("join round-trips a source that uses single blank-line separators", () => {
    const src =
      "Para one\nstill one\n\n# Head\n\n- a\n- b\n\n| x | y |\n| --- | --- |\n\n```\nfenced\n\ninside blank\n```";
    expect(joinMarkdownBlocks(splitMarkdownBlocks(src))).toBe(src);
  });

  it("splitting is idempotent through join", () => {
    const src = "a\n\n```\nx\n\ny\n```\n\nb";
    const once = splitMarkdownBlocks(src);
    expect(splitMarkdownBlocks(joinMarkdownBlocks(once))).toEqual(once);
  });
});

describe("joinMarkdownBlocks", () => {
  it("joins blocks with a blank line", () => {
    expect(joinMarkdownBlocks(["a", "b"])).toBe("a\n\nb");
    expect(joinMarkdownBlocks([])).toBe("");
    expect(joinMarkdownBlocks(["only"])).toBe("only");
  });
});

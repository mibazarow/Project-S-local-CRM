import { describe, expect, it } from "vitest";
import {
  joinMarkdownBlocks,
  renderMarkdown,
  splitMarkdownBlocks,
  splitMarkdownBlocksWithRanges,
} from "./markdown";

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

describe("splitMarkdownBlocksWithRanges", () => {
  it("ranges cover only block content, keeping blank lines outside", () => {
    const src = "\na\n\n\n\nb\n\n";
    const info = splitMarkdownBlocksWithRanges(src);
    expect(info.map((b) => b.text)).toEqual(["a", "b"]);
    expect(info.map((b) => src.slice(b.from, b.to))).toEqual(["a", "b"]);
  });

  it("editing one block in place keeps runs of blank lines and trailing blanks", () => {
    const src = "\none\n\n\n\n# Head\n\nlast\n\n\n";
    const info = splitMarkdownBlocksWithRanges(src);
    const edited = src.slice(0, info[1].from) + "edited" + src.slice(info[1].to);
    expect(edited).toBe("\none\n\n\n\nedited\n\nlast\n\n\n");
    expect(splitMarkdownBlocks(edited)).toEqual(["one", "edited", "last"]);
  });

  it("deleting a block with its following gap leaves other blank lines alone", () => {
    const src = "a\n\n\nb\n\n\n\nc";
    const info = splitMarkdownBlocksWithRanges(src);
    const deleted = src.slice(0, info[1].from) + src.slice(info[2].from);
    expect(deleted).toBe("a\n\n\nc");
  });

  it("ranges keep a fenced code block with blank lines inside as one block", () => {
    const src = "para\n\n```\nx\n\ny\n```\n\n";
    const info = splitMarkdownBlocksWithRanges(src);
    expect(info.map((b) => b.text)).toEqual(["para", "```\nx\n\ny\n```"]);
    expect(src.slice(info[1].from, info[1].to)).toBe("```\nx\n\ny\n```");
  });

  it("ranges match splitMarkdownBlocks texts", () => {
    const src = "a\nb\n\n\n  ```\nfenced\n\ninside\n  ```\n\nc\n";
    const info = splitMarkdownBlocksWithRanges(src);
    expect(info.map((b) => src.slice(b.from, b.to))).toEqual(splitMarkdownBlocks(src));
  });
});

describe("renderMarkdown", () => {
  it("renders single newlines as <br> with breaks: true", () => {
    expect(renderMarkdown("a\nb", { breaks: true })).toContain("<br");
  });

  it("keeps a soft-wrapped paragraph as one line without breaks", () => {
    expect(renderMarkdown("a\nb")).not.toContain("<br");
  });
});

import { describe, expect, it } from "vitest";
import { stripInlineToolCalls } from "./inlineToolCalls";

describe("stripInlineToolCalls", () => {
  it("removes the reported dialect and keeps the prose", () => {
    const text =
      "Сейчас проверю.\n<tool_call>\n<function=webfetch>\n<parameter=url>\nhttps://e.ru\n</parameter>\n</tool_call>\nГотово.";
    expect(stripInlineToolCalls(text)).toBe("Сейчас проверю.\n\nГотово.");
  });

  it("removes several blocks", () => {
    const text =
      "<tool_call><function=a><parameter=x>1</parameter></tool_call><tool_call><function=b></tool_call>хвост";
    expect(stripInlineToolCalls(text)).toBe("хвост");
  });

  it("hides an unterminated block (still streaming)", () => {
    expect(stripInlineToolCalls("Думаю.\n<tool_call><function=bash>")).toBe("Думаю.");
  });

  it("leaves plain text untouched, including whitespace", () => {
    expect(stripInlineToolCalls("Первая ")).toBe("Первая ");
    expect(stripInlineToolCalls("")).toBe("");
  });
});

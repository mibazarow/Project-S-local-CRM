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

  it("removes the pipe and function_calls dialects", () => {
    expect(stripInlineToolCalls("a<|tool_call|><function=x><|/tool_call|>b")).toBe("ab");
    expect(
      stripInlineToolCalls(
        'a<function_calls><invoke name="x"><parameter name="k">v</parameter></invoke></function_calls>b',
      ),
    ).toBe("ab");
  });

  it("removes a bare function tag", () => {
    expect(stripInlineToolCalls("до <function=webfetch><parameter=url>u</parameter></function> после")).toBe(
      "до  после",
    );
  });

  it("never strips inside a code fence", () => {
    const sample = "Вот пример:\n```\n<tool_call><function=bash><parameter=command>ls</parameter></tool_call>\n```";
    expect(stripInlineToolCalls(sample)).toBe(sample);
    // Outside the fence the markup still goes away.
    const mixed = "до <tool_call><function=x></tool_call> ```code``` после";
    expect(stripInlineToolCalls(mixed)).toBe("до  ```code``` после");
  });

  it("leaves plain text and code samples untouched", () => {
    expect(stripInlineToolCalls("Первая ")).toBe("Первая ");
    expect(stripInlineToolCalls("")).toBe("");
    const json = '```json\n{"name":"webfetch"}\n```';
    expect(stripInlineToolCalls(json)).toBe(json);
  });
});

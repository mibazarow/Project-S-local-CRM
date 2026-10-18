import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import {
  chatReducer,
  combineQueued,
  countMatches,
  historyToItems,
  summarizeArgs,
  toMessage,
  type ChatAction,
  type ChatState,
} from "./chatShared";
import type { MessageDto } from "../../api/types";

const KEY = 1;

function dto(over: Partial<MessageDto> & Pick<MessageDto, "id" | "role">): MessageDto {
  return {
    chatId: 10,
    content: null,
    reasoning: null,
    toolCalls: [],
    toolCallId: null,
    name: null,
    createdAt: "2026-01-01T00:00:00Z",
    position: 0,
    attachments: [],
    pinned: false,
    ...over,
  };
}

function apply(state: ChatState, ...actions: ChatAction[]): ChatState {
  return actions.reduce((acc, a) => chatReducer(acc, a), state);
}

describe("chatReducer", () => {
  it("delta opens a remote run on the first event and appends text", () => {
    let s = chatReducer({}, { type: "delta", key: KEY, text: "He" });
    expect(s[KEY].run).toEqual({
      text: "He",
      reasoning: "",
      steps: [],
      compactions: [],
      error: null,
      local: false,
    });
    s = chatReducer(s, { type: "delta", key: KEY, text: "llo" });
    expect(s[KEY].run?.text).toBe("Hello");
  });

  it("reasoningDelta opens a run and appends to reasoning only", () => {
    let s = chatReducer({}, { type: "reasoningDelta", key: KEY, text: "th" });
    expect(s[KEY].run).toEqual({
      text: "",
      reasoning: "th",
      steps: [],
      compactions: [],
      error: null,
      local: false,
    });
    s = apply(s, { type: "delta", key: KEY, text: "a" }, { type: "reasoningDelta", key: KEY, text: "ink" });
    expect(s[KEY].run).toEqual({
      text: "a",
      reasoning: "think",
      steps: [],
      compactions: [],
      error: null,
      local: false,
    });
  });

  it("send appends the user item and opens a local run; delta keeps it local", () => {
    const att = { name: "a.png", savedPath: "p/a.png", kind: "image" as const, isImage: true };
    let s = chatReducer({}, { type: "send", key: KEY, text: "hi", attachments: [att] });
    expect(s[KEY].items).toHaveLength(1);
    expect(s[KEY].items[0]).toEqual({
      key: expect.any(Number),
      role: "user",
      text: "hi",
      reasoning: "",
      attachments: [att],
      messageIds: [],
      pinned: false,
      steps: [],
      compactions: [],
      note: null,
      error: null,
    });
    expect(s[KEY].run).toEqual({
      text: "",
      reasoning: "",
      steps: [],
      compactions: [],
      error: null,
      local: true,
    });
    s = chatReducer(s, { type: "delta", key: KEY, text: "x" });
    expect(s[KEY].run).toEqual({
      text: "x",
      reasoning: "",
      steps: [],
      compactions: [],
      error: null,
      local: true,
    });
  });

  it("stepStart opens a run when none exists and appends steps", () => {
    let s = chatReducer({}, { type: "stepStart", key: KEY, name: "read_file", summary: "path=a" });
    expect(s[KEY].run?.local).toBe(false);
    expect(s[KEY].run?.steps).toEqual([
      { key: expect.any(Number), name: "read_file", summary: "path=a", status: "run" },
    ]);
    s = chatReducer(s, { type: "stepStart", key: KEY, name: "write_file", summary: "" });
    expect(s[KEY].run?.steps.map((st) => st.name)).toEqual(["read_file", "write_file"]);
  });

  it("stepFinish marks the oldest open step, ok or err", () => {
    let s = apply(
      {},
      { type: "stepStart", key: KEY, name: "a", summary: "" },
      { type: "stepStart", key: KEY, name: "b", summary: "" },
      { type: "stepFinish", key: KEY, ok: true },
    );
    expect(s[KEY].run?.steps.map((st) => st.status)).toEqual(["ok", "run"]);
    s = chatReducer(s, { type: "stepFinish", key: KEY, ok: false });
    expect(s[KEY].run?.steps.map((st) => st.status)).toEqual(["ok", "err"]);
    s = chatReducer(s, { type: "stepFinish", key: KEY, ok: true });
    expect(s[KEY].run?.steps.map((st) => st.status)).toEqual(["ok", "err"]);
  });

  it("stepFinish without a run leaves the slice without a run", () => {
    const s = chatReducer({}, { type: "stepFinish", key: KEY, ok: true });
    expect(s[KEY].run).toBeNull();
    expect(s[KEY].items).toEqual([]);
  });

  it("done finalizes a local run: trims text, appends a differing answer after a blank line", () => {
    let s = apply(
      {},
      { type: "send", key: KEY, text: "hi", attachments: [] },
      { type: "delta", key: KEY, text: " partial " },
      { type: "done", key: KEY, answer: "full", reason: "finished", reasoning: null },
    );
    expect(s[KEY].run).toBeNull();
    expect(s[KEY].items).toHaveLength(2);
    const item = s[KEY].items[1];
    expect(item.role).toBe("agent");
    expect(item.text).toBe("partial\n\nfull");
    expect(item.reasoning).toBe("");
    expect(item.note).toBeNull();
    expect(item.error).toBeNull();
    expect(item.attachments).toEqual([]);
    expect(item.messageIds).toEqual([]);
  });

  it("done with answer equal to the streamed text keeps a single part", () => {
    let s = apply(
      {},
      { type: "send", key: KEY, text: "hi", attachments: [] },
      { type: "delta", key: KEY, text: " same " },
      { type: "done", key: KEY, answer: "same", reason: "finished", reasoning: null },
    );
    expect(s[KEY].items[1].text).toBe("same");
  });

  it("done maps reason to a note: step_limit, cancelled, otherwise null", () => {
    const finish = (reason: string) => {
      const s = apply(
        {},
        { type: "send", key: KEY, text: "hi", attachments: [] },
        { type: "done", key: KEY, answer: "a", reason, reasoning: null },
      );
      return s[KEY].items[1].note;
    };
    expect(finish("step_limit")).toBe("Лимит шагов достигнут — работа остановлена.");
    expect(finish("cancelled")).toBe("Работа остановлена.");
    expect(finish("finished")).toBeNull();
  });

  it("done prefers the event reasoning and falls back to the streamed one", () => {
    const withEvent = apply(
      {},
      { type: "send", key: KEY, text: "hi", attachments: [] },
      { type: "reasoningDelta", key: KEY, text: "live" },
      { type: "done", key: KEY, answer: "", reason: "finished", reasoning: " full " },
    );
    expect(withEvent[KEY].items[1].reasoning).toBe("full");
    const fallback = apply(
      {},
      { type: "send", key: KEY, text: "hi", attachments: [] },
      { type: "reasoningDelta", key: KEY, text: " live " },
      { type: "done", key: KEY, answer: "", reason: "finished", reasoning: null },
    );
    expect(fallback[KEY].items[1].reasoning).toBe("live");
  });

  it("done on a remote run drops the live slice without adding an item", () => {
    const s = apply(
      {},
      { type: "delta", key: KEY, text: "x" },
      { type: "done", key: KEY, answer: "x", reason: "finished", reasoning: null },
    );
    expect(s[KEY].run).toBeNull();
    expect(s[KEY].items).toEqual([]);
  });

  it("done without a run is a no-op", () => {
    const s = chatReducer({}, { type: "done", key: KEY, answer: "a", reason: "finished", reasoning: null });
    expect(s[KEY].run).toBeNull();
    expect(s[KEY].items).toEqual([]);
  });

  it("runError on a local run appends an error item carrying steps and compactions", () => {
    let s = apply(
      {},
      { type: "send", key: KEY, text: "hi", attachments: [] },
      { type: "delta", key: KEY, text: " half " },
      { type: "reasoningDelta", key: KEY, text: "why" },
      { type: "stepStart", key: KEY, name: "t", summary: "" },
      { type: "compaction", key: KEY, summary: "sum" },
      { type: "runError", key: KEY, message: "boom" },
    );
    expect(s[KEY].run).toBeNull();
    expect(s[KEY].items).toHaveLength(2);
    const item = s[KEY].items[1];
    expect(item.role).toBe("agent");
    expect(item.text).toBe("half");
    expect(item.reasoning).toBe("why");
    expect(item.error).toBe("boom");
    expect(item.steps).toHaveLength(1);
    expect(item.steps[0].status).toBe("run");
    expect(item.compactions).toHaveLength(1);
  });

  it("runError on a remote run drops the run without an item; no-op without a run", () => {
    const remote = apply(
      {},
      { type: "delta", key: KEY, text: "x" },
      { type: "runError", key: KEY, message: "boom" },
    );
    expect(remote[KEY].run).toBeNull();
    expect(remote[KEY].items).toEqual([]);
    const none = chatReducer({}, { type: "runError", key: KEY, message: "boom" });
    expect(none[KEY].run).toBeNull();
    expect(none[KEY].items).toEqual([]);
  });

  it("enqueue appends to the per-chat queue, clearQueue empties it", () => {
    let s = apply({}, { type: "enqueue", key: KEY, text: "a" }, { type: "enqueue", key: KEY, text: "b" });
    expect(s[KEY].queue).toEqual(["a", "b"]);
    s = chatReducer(s, { type: "clearQueue", key: KEY });
    expect(s[KEY].queue).toEqual([]);
  });

  it("combineQueued keeps a single message verbatim and joins several with a divider", () => {
    expect(combineQueued([])).toBe("");
    expect(combineQueued(["a"])).toBe("a");
    expect(combineQueued(["a", "b"])).toBe("a\n\n---\n\nb");
  });

  it("compaction appends to the live run and opens one when absent", () => {
    let s = chatReducer({}, { type: "compaction", key: KEY, summary: "s1" });
    expect(s[KEY].run?.local).toBe(false);
    expect(s[KEY].run?.compactions).toEqual([{ key: expect.any(Number), summary: "s1" }]);
    s = chatReducer(s, { type: "compaction", key: KEY, summary: "s2" });
    expect(s[KEY].run?.compactions.map((c) => c.summary)).toEqual(["s1", "s2"]);
  });

  it("item and step keys are unique and strictly increasing", () => {
    const s = apply(
      {},
      { type: "send", key: KEY, text: "hi", attachments: [] },
      { type: "stepStart", key: KEY, name: "a", summary: "" },
      { type: "done", key: KEY, answer: "x", reason: "finished", reasoning: null },
    );
    const keys = [
      s[KEY].items[0].key,
      s[KEY].items[1].steps[0].key,
      s[KEY].items[1].key,
    ];
    expect(new Set(keys).size).toBe(3);
    expect(keys[0]).toBeLessThan(keys[1]);
    expect(keys[1]).toBeLessThan(keys[2]);
  });
});

describe("historyToItems", () => {
  it("maps a user message to a user item with pinned flag and DB id", () => {
    const items = historyToItems([dto({ id: 5, role: "user", content: "вопрос", pinned: true })]);
    expect(items).toHaveLength(1);
    expect(items[0].role).toBe("user");
    expect(items[0].text).toBe("вопрос");
    expect(items[0].messageIds).toEqual([5]);
    expect(items[0].pinned).toBe(true);
    expect(items[0].attachments).toEqual([]);
    expect(items[0].synthetic).toBeUndefined();
  });

  it("user with null content becomes empty text and attachments pass through", () => {
    const att = { name: "a.png", savedPath: "p/a.png", kind: "image" as const, isImage: true };
    const items = historyToItems([dto({ id: 1, role: "user", content: null, attachments: [att] })]);
    expect(items[0].text).toBe("");
    expect(items[0].attachments).toEqual([att]);
  });

  it("merges consecutive assistant messages into one agent bubble joined by a blank line; last pinned wins", () => {
    const items = historyToItems([
      dto({ id: 1, role: "assistant", content: "a1", pinned: true }),
      dto({ id: 2, role: "assistant", content: "a2", pinned: false, reasoning: "r2" }),
    ]);
    expect(items).toHaveLength(1);
    expect(items[0].role).toBe("agent");
    expect(items[0].text).toBe("a1\n\na2");
    expect(items[0].reasoning).toBe("r2");
    expect(items[0].messageIds).toEqual([1, 2]);
    expect(items[0].pinned).toBe(false);
  });

  it("joins reasoning of several assistant messages with a blank line", () => {
    const items = historyToItems([
      dto({ id: 1, role: "assistant", content: null, reasoning: "r1" }),
      dto({ id: 2, role: "assistant", content: null, reasoning: "r2" }),
    ]);
    expect(items).toHaveLength(1);
    expect(items[0].text).toBe("");
    expect(items[0].reasoning).toBe("r1\n\nr2");
  });

  it("tool calls become steps with summarized args; tool results resolve status by toolCallId", () => {
    const items = historyToItems([
      dto({
        id: 1,
        role: "assistant",
        content: null,
        toolCalls: [
          { id: "t1", name: "read_file", arguments: '{"path":"a.txt"}' },
          { id: "t2", name: "write_file", arguments: '{"path":"b.txt","content":"x"}' },
        ],
      }),
      dto({ id: 2, role: "tool", toolCallId: "t1", content: '{"ok":true}' }),
      dto({ id: 3, role: "tool", toolCallId: "t2", content: '{"ok":false,"error":"x"}' }),
    ]);
    expect(items).toHaveLength(1);
    expect(items[0].steps.map((st) => [st.name, st.summary, st.status])).toEqual([
      ["read_file", "path=a.txt", "ok"],
      ["write_file", "path=b.txt, content=x", "err"],
    ]);
    expect(items[0].messageIds).toEqual([1, 2, 3]);
  });

  it("tool result with null or non-JSON content counts as ok; unknown toolCallId leaves the step running", () => {
    const items = historyToItems([
      dto({ id: 1, role: "assistant", content: null, toolCalls: [
        { id: "t1", name: "a", arguments: "{}" },
        { id: "t2", name: "b", arguments: "{}" },
        { id: "t3", name: "c", arguments: "{}" },
      ] }),
      dto({ id: 2, role: "tool", toolCallId: "t1", content: null }),
      dto({ id: 3, role: "tool", toolCallId: "t2", content: "plain text" }),
      dto({ id: 4, role: "tool", toolCallId: "zzz", content: '{"ok":false}' }),
    ]);
    expect(items[0].steps.map((st) => st.status)).toEqual(["ok", "ok", "run"]);
  });

  it("assistant attachments get the owning messageId and a 0-based per-message index", () => {
    const items = historyToItems([
      dto({
        id: 7,
        role: "assistant",
        content: "файл",
        attachments: [
          { name: "a.png", savedPath: "p/a.png", kind: "image", isImage: true },
          { name: "b.pdf", savedPath: "p/b.pdf", kind: "document", isImage: false },
        ],
      }),
    ]);
    expect(items[0].attachments).toEqual([
      { name: "a.png", savedPath: "p/a.png", kind: "image", isImage: true, messageId: 7, index: 0 },
      { name: "b.pdf", savedPath: "p/b.pdf", kind: "document", isImage: false, messageId: 7, index: 1 },
    ]);
  });

  it("user flushes the agent group; empty assistant groups are dropped; leading tool messages are ignored", () => {
    const items = historyToItems([
      dto({ id: 1, role: "tool", toolCallId: "x", content: "{}" }),
      dto({ id: 2, role: "assistant", content: "a" }),
      dto({ id: 3, role: "assistant", content: null }),
      dto({ id: 4, role: "user", content: "u" }),
      dto({ id: 5, role: "assistant", content: null }),
    ]);
    expect(items.map((it) => it.role)).toEqual(["agent", "user"]);
    expect(items[0].messageIds).toEqual([2, 3]);
    expect(items[1].messageIds).toEqual([4]);
  });

  it("item keys are unique and strictly increasing", () => {
    const items = historyToItems([
      dto({ id: 1, role: "user", content: "u" }),
      dto({ id: 2, role: "assistant", content: "a" }),
      dto({ id: 3, role: "user", content: "u2" }),
    ]);
    const keys = items.map((it) => it.key);
    expect(new Set(keys).size).toBe(3);
    expect(keys).toEqual([...keys].sort((a, b) => a - b));
  });
});

describe("chat UX helpers", () => {
  it("toMessage unwraps a non-empty message field, keeps strings, falls back", () => {
    expect(toMessage({ message: "oops" })).toBe("oops");
    expect(toMessage({ message: "" })).toBe("Неизвестная ошибка");
    expect(toMessage("plain")).toBe("plain");
    expect(toMessage(null)).toBe("Неизвестная ошибка");
    expect(toMessage(42)).toBe("Неизвестная ошибка");
  });

  it("summarizeArgs renders k=v pairs, parses JSON strings, truncates", () => {
    expect(summarizeArgs('{"path":"a.txt"}')).toBe("path=a.txt");
    expect(summarizeArgs({ a: 1, b: "x" })).toBe("a=1, b=x");
    expect(summarizeArgs("not json")).toBe("not json");
    expect(summarizeArgs(null)).toBe("");
    expect(summarizeArgs("x".repeat(200))).toBe("x".repeat(120));
    const many = summarizeArgs({ a: "1", b: "2", c: "3", d: "4", e: "5" });
    expect(many).toBe("a=1, b=2, c=3, d=4");
  });

  it("countMatches counts case-insensitive occurrences across item texts", () => {
    const items = historyToItems([
      dto({ id: 1, role: "user", content: "Ab ab AB" }),
      dto({ id: 2, role: "assistant", content: "xx ab yy" }),
    ]);
    expect(countMatches(items, " ab ")).toBe(4);
    expect(countMatches(items, "ab")).toBe(4);
    expect(countMatches(items, "  ")).toBe(0);
    expect(countMatches(items, "")).toBe(0);
  });
});

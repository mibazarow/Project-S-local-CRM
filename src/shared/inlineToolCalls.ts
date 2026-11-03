// Display guard for tool calls written as text instead of the native
// `tool_calls` field. The backend parses these into real calls
// (llm::inline_tools) and strips them from the stored message, but transcripts
// written before that fix still contain the markup, and a half-streamed block
// can flash before its closing tag arrives. Stripping at render time keeps the
// content and the «Размышления» block readable.
//
// Code fences are left untouched: the model may quote the format when the user
// asks about it, and a quoted example must be readable, not erased.
const BLOCKS: ReadonlyArray<readonly [string, string]> = [
  ["<tool_call>", "</tool_call>"],
  ["<|tool_call|>", "<|/tool_call|>"],
  ["<function_calls>", "</function_calls>"],
];

export function stripInlineToolCalls(text: string): string {
  let touched = false;
  const parts = text.split(/(```[\s\S]*?(?:```|$))/);
  const out = parts
    .map((part, index) => {
      // Odd indexes are the captured fences.
      if (index % 2 === 1) return part;
      const [next, changed] = stripText(part);
      touched ||= changed;
      return next;
    })
    .join("");
  return touched ? out.trim() : text;
}

function stripText(text: string): [string, boolean] {
  let out = text;
  let touched = false;
  for (const [open, close] of BLOCKS) {
    const [next, changed] = stripBlocks(out, open, close);
    out = next;
    touched ||= changed;
  }
  const [next, changed] = stripBareFunction(out);
  return [next, touched || changed];
}

// Removes every `open … close` pair; an unterminated pair eats the rest (that
// is a call still streaming in).
function stripBlocks(text: string, open: string, close: string): [string, boolean] {
  if (!text.includes(open)) return [text, false];
  let out = "";
  let rest = text;
  for (;;) {
    const start = rest.indexOf(open);
    if (start === -1) {
      out += rest;
      break;
    }
    out += rest.slice(0, start);
    const after = rest.slice(start + open.length);
    const end = after.indexOf(close);
    if (end === -1) break;
    rest = after.slice(end + close.length);
  }
  return [out, true];
}

// A bare `<function=name>…</function>` without the wrapper tags.
function stripBareFunction(text: string): [string, boolean] {
  const open = /<function(?:=|\s+name=)/.exec(text);
  if (!open) return [text, false];
  const start = open.index;
  const after = text.slice(start);
  const close = after.indexOf("</function>");
  const end = close === -1 ? after.length : close + "</function>".length;
  return [text.slice(0, start) + after.slice(end), true];
}

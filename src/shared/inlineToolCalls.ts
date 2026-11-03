// Display guard for the inline tool-call dialect some gateways emit as text:
//
//   <tool_call>
//   <function=webfetch>
//   <parameter=url>https://…</parameter>
//   </tool_call>
//
// The backend now parses these into real tool calls (llm::inline_tools), but
// transcripts written before that fix still contain the raw markup, and a
// half-streamed block can flash before the closing tag arrives. Stripping at
// render time keeps both the content and the «Размышления» block readable.
const OPEN = "<tool_call>";
const CLOSE = "</tool_call>";

export function stripInlineToolCalls(text: string): string {
  if (!text.includes(OPEN)) return text;
  let out = "";
  let rest = text;
  for (;;) {
    const start = rest.indexOf(OPEN);
    if (start === -1) {
      out += rest;
      break;
    }
    out += rest.slice(0, start);
    const after = rest.slice(start + OPEN.length);
    const close = after.indexOf(CLOSE);
    // Unterminated block: the rest is markup (or a partial that is still
    // streaming in), so nothing of it should be shown.
    if (close === -1) break;
    rest = after.slice(close + CLOSE.length);
  }
  return out.trim();
}

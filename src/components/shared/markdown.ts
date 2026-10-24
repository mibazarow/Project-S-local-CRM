import { marked, type MarkedOptions } from "marked";
import DOMPurify from "dompurify";

// DOMPurify's default URI allowlist strips custom schemes, so a `note:`/`notes:`
// link would lose its href. Mirror the default IS_ALLOWED_URI and add `notes?`
// so explicit note links survive sanitization; every other scheme is unchanged.
const MARKDOWN_SANITIZE_CONFIG = {
  ALLOWED_URI_REGEXP:
    /^(?:(?:(?:f|ht)tps?|mailto|tel|callto|sms|cid|xmpp|matrix|notes?):|[^a-z]|[a-z+.-]+(?:[^a-z+.:-]|$))/i,
};

// Shared sanitized markdown renderer (marked + DOMPurify), used by chat
// bubbles and the notes block editor. DOMPurify's default allowlist keeps
// inline HTML like <u>, so underline formatting survives sanitization.
export function renderMarkdown(source: string, options?: MarkedOptions): string {
  return DOMPurify.sanitize(marked.parse(source, { ...options, async: false }), MARKDOWN_SANITIZE_CONFIG);
}

export interface MarkdownBlockRange {
  text: string;
  // Offset range in the original source covering exactly the block content;
  // blank lines around the block stay outside [from, to).
  from: number;
  to: number;
}

// Top-level markdown blocks with their offsets in the source: split on blank
// lines, but never inside a fenced code block (``` / ~~~). Lists, tables and
// blockquotes without blank lines stay in one block; a blank line inside a
// fence keeps the fence intact. Ranges let callers replace a single block in
// place, so untouched blank-line structure (runs, leading/trailing blanks)
// survives an edit instead of being re-joined with exactly two newlines.
export function splitMarkdownBlocksWithRanges(source: string): MarkdownBlockRange[] {
  const lines = source.split("\n");
  const blocks: MarkdownBlockRange[] = [];
  let current: string[] = [];
  let fence: string | null = null;
  let blockFrom = 0;
  let offset = 0;
  const flush = (to: number) => {
    if (current.length > 0) {
      blocks.push({ text: current.join("\n"), from: blockFrom, to });
      current = [];
    }
  };
  for (const line of lines) {
    const marker = line.trimStart().match(/^(`{3,}|~{3,})/);
    if (fence !== null) {
      current.push(line);
      if (marker && marker[1][0] === fence[0] && marker[1].length >= fence.length) {
        fence = null;
      }
    } else if (marker) {
      if (current.length === 0) blockFrom = offset;
      fence = marker[1];
      current.push(line);
    } else if (line.trim() === "") {
      flush(Math.max(offset - 1, 0));
    } else {
      if (current.length === 0) blockFrom = offset;
      current.push(line);
    }
    offset += line.length + 1;
  }
  flush(source.length);
  return blocks;
}

export function splitMarkdownBlocks(source: string): string[] {
  return splitMarkdownBlocksWithRanges(source).map((b) => b.text);
}

export function joinMarkdownBlocks(blocks: string[]): string {
  return blocks.join("\n\n");
}

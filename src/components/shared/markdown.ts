import { marked, type MarkedOptions } from "marked";
import DOMPurify from "dompurify";

// Shared sanitized markdown renderer (marked + DOMPurify), used by chat
// bubbles and the notes block editor. DOMPurify's default allowlist keeps
// inline HTML like <u>, so underline formatting survives sanitization.
export function renderMarkdown(source: string, options?: MarkedOptions): string {
  return DOMPurify.sanitize(marked.parse(source, { ...options, async: false }));
}

// Top-level markdown blocks: split on blank lines, but never inside a fenced
// code block (``` / ~~~). Lists, tables and blockquotes without blank lines
// stay in one block; a blank line inside a fence keeps the fence intact.
export function splitMarkdownBlocks(source: string): string[] {
  const lines = source.split("\n");
  const blocks: string[] = [];
  let current: string[] = [];
  let fence: string | null = null;
  const flush = () => {
    if (current.length > 0) {
      blocks.push(current.join("\n"));
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
      continue;
    }
    if (marker) {
      fence = marker[1];
      current.push(line);
      continue;
    }
    if (line.trim() === "") {
      flush();
      continue;
    }
    current.push(line);
  }
  flush();
  return blocks;
}

export function joinMarkdownBlocks(blocks: string[]): string {
  return blocks.join("\n\n");
}

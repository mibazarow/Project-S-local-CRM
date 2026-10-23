import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

// Reads the CSS entry plus every @imported file in source order, so guard
// assertions see the exact cascade the browser gets after the R-21 split.
// shortcut: concatenates @imported files only; rules written directly in
// app.css (none today) would be skipped — inline them into a split file instead.
export function readAppCss(): string {
  const entry = readFileSync(join(HERE, "app.css"), "utf8");
  const parts: string[] = [];
  for (const line of entry.split("\n")) {
    const m = /^@import\s+["']([^"']+)["'];/.exec(line);
    if (m) parts.push(readFileSync(join(HERE, m[1]), "utf8"));
  }
  return parts.join("\n");
}

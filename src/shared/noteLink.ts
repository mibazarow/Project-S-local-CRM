// Conservative classifier for markdown links in agent answers: decides whether
// an <a href> points at a note in the Notes/ vault (so the click opens it inside
// «Заметки и файлы») or must keep its default behavior (external browser, project
// file, image, in-page anchor). Returns the vault-relative path for a note link,
// otherwise null.
//
// Treated as a note:
//   - explicit `note:` / `notes:` scheme (scheme stripped, remainder is the path);
//   - a relative path ending in `.md`/`.markdown` with no `..` traversal;
//   - an absolute path inside the vault (has a `/Notes/` segment → part after it).
// Everything else (http/https, mailto, tel, other schemes, non-`.md` files,
// images, `#…` anchors) is left to the webview default.
export function resolveNoteLink(href: string): string | null {
  const raw = href.trim();
  if (!raw) return null;

  const scheme = raw.match(/^([a-z][a-z0-9+.-]*):/i);
  if (scheme) {
    const name = scheme[1].toLowerCase();
    if (name !== "note" && name !== "notes") return null;
    const rest = safeDecode(raw.slice(scheme[1].length + 1)).trim();
    return rest || null;
  }

  const decoded = safeDecode(raw);
  if (decoded.startsWith("/")) {
    const marker = "/Notes/";
    const at = decoded.indexOf(marker);
    if (at === -1) return null;
    const rel = decoded.slice(at + marker.length).replace(/^\/+/, "").trim();
    return rel || null;
  }

  const rel = decoded.replace(/^\.\/+/, "").trim();
  if (!rel || rel.split("/").some((seg) => seg === "..")) return null;
  return /\.(md|markdown)$/i.test(rel) ? rel : null;
}

function safeDecode(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

// Single app-wide note-link handler: App registers it (requestNote + switch to
// the notes tab); MarkdownMessage calls it on a note-link click. Mirrors the
// module-level toast channel so deep chat bubbles need no context or props.
type NoteLinkHandler = (relPath: string) => void;
let handler: NoteLinkHandler | null = null;

export function setNoteLinkHandler(next: NoteLinkHandler | null): void {
  handler = next;
}

export function openNoteLink(relPath: string): void {
  handler?.(relPath);
}

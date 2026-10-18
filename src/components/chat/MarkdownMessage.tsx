import { memo, useEffect, useMemo, useRef, useState, type MouseEvent } from "react";
import { renderMarkdown } from "../shared/markdown";
import { openNoteLink, resolveNoteLink } from "../../shared/noteLink";

const COPIED_MS = 1500;

// Clipboard write with the legacy execCommand fallback (WKWebView without a
// focused-clipboard grant).
async function copyToClipboard(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through to the legacy path
  }
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

// Flash a DOM copy button (code blocks live in raw HTML, outside React):
// «Скопировано» for COPIED_MS, then the original label comes back.
const codeCopyTimers = new WeakMap<Element, number>();

function flashCopied(btn: HTMLElement): void {
  if (btn.dataset.label === undefined) btn.dataset.label = btn.textContent ?? "";
  btn.textContent = "Скопировано";
  btn.classList.add("copied");
  const prev = codeCopyTimers.get(btn);
  if (prev !== undefined) window.clearTimeout(prev);
  codeCopyTimers.set(
    btn,
    window.setTimeout(() => {
      btn.textContent = btn.dataset.label ?? "";
      btn.classList.remove("copied");
      codeCopyTimers.delete(btn);
    }, COPIED_MS),
  );
}

// Wrap every fenced code block in a container carrying its own copy button.
// Runs after sanitization, so the injected button is never stripped; the
// button text is static and the code text is read from the DOM on click.
function decorateCodeBlocks(html: string): string {
  const doc = new DOMParser().parseFromString(`<div>${html}</div>`, "text/html");
  const root = doc.body.firstElementChild;
  if (!root) return html;
  for (const pre of Array.from(root.querySelectorAll("pre"))) {
    const wrap = doc.createElement("div");
    wrap.className = "md-code";
    const btn = doc.createElement("button");
    btn.type = "button";
    btn.className = "md-code-copy";
    btn.textContent = "Скопировать";
    pre.replaceWith(wrap);
    wrap.append(pre, btn);
  }
  return root.innerHTML;
}

// An agent reply rendered as sanitized Markdown with copy buttons: one for
// the whole message (copies the RAW text) and one per code block. Re-renders
// incrementally on streaming deltas; unclosed fences are auto-closed by
// marked, so partial markdown never breaks the bubble.
export const MarkdownMessage = memo(function MarkdownMessage({ text }: { text: string }) {
  const html = useMemo(
    () => decorateCodeBlocks(renderMarkdown(text, { breaks: true })),
    [text],
  );
  const [copied, setCopied] = useState(false);
  const timerRef = useRef(0);
  useEffect(() => () => window.clearTimeout(timerRef.current), []);

  const copyWhole = () => {
    void copyToClipboard(text).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.clearTimeout(timerRef.current);
      timerRef.current = window.setTimeout(() => setCopied(false), COPIED_MS);
    });
  };

  const onContentClick = (e: MouseEvent<HTMLDivElement>) => {
    const target = e.target as HTMLElement;
    const anchor = target.closest<HTMLAnchorElement>("a[href]");
    if (anchor) {
      const href = anchor.getAttribute("href");
      const rel = href ? resolveNoteLink(href) : null;
      if (rel !== null) {
        e.preventDefault();
        openNoteLink(rel);
      }
      return;
    }
    const btn = target.closest<HTMLElement>("button.md-code-copy");
    if (!btn) return;
    const code = btn.parentElement?.querySelector("pre")?.textContent ?? "";
    void copyToClipboard(code.trim()).then((ok) => {
      if (ok) flashCopied(btn);
    });
  };

  return (
    <div className="bubble md">
      <button
        className={copied ? "msg-copy copied" : "msg-copy"}
        type="button"
        onClick={copyWhole}
      >
        {copied ? "Скопировано" : "Скопировать"}
      </button>
      <div
        className="preview-body md"
        onClick={onContentClick}
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </div>
  );
});

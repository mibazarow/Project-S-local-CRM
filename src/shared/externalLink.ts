// External-link handling for the Tauri WebView: a plain navigation would
// replace the app with the target site and leave no way back, so http(s),
// mailto and tel links are handed to the OS default browser/handler instead.
//
// Only absolute URLs with an allowed scheme are external. Note links, in-page
// anchors, relative paths and every other scheme (javascript:, data:, file: …)
// return null and keep whatever behavior their own handler gives them.
import { openUrl } from "@tauri-apps/plugin-opener";

const EXTERNAL_SCHEMES = new Set(["http", "https", "mailto", "tel"]);

export function externalHref(href: string, base?: string): string | null {
  const raw = href.trim();
  if (!raw || raw.startsWith("#")) return null;

  const scheme = raw.match(/^([a-z][a-z0-9+.-]*):/i);
  if (!scheme) return null;
  if (!EXTERNAL_SCHEMES.has(scheme[1].toLowerCase())) return null;

  try {
    const url = new URL(raw, base);
    return EXTERNAL_SCHEMES.has(url.protocol.slice(0, -1)) ? url.href : null;
  } catch {
    return null;
  }
}

type Opener = (url: string) => Promise<void>;

function anchorFrom(target: EventTarget | null): HTMLAnchorElement | null {
  if (!(target instanceof Node)) return null;
  const el = target.nodeType === Node.ELEMENT_NODE ? (target as Element) : target.parentElement;
  const anchor = el?.closest?.("a[href]");
  return anchor instanceof HTMLAnchorElement ? anchor : null;
}

export function installExternalLinkHandler(open: Opener = openUrl): () => void {
  const onClick = (event: MouseEvent) => {
    const middle = event.type === "auxclick";
    if (middle ? event.button !== 1 : event.button !== 0) return;
    if (event.defaultPrevented) return;

    const anchor = anchorFrom(event.target);
    if (!anchor) return;

    const href = anchor.getAttribute("href");
    if (!href) return;

    const url = externalHref(href);
    if (!url) return;

    event.preventDefault();
    event.stopPropagation();
    try {
      void open(url).catch((err) => console.warn("Failed to open external link", url, err));
    } catch (err) {
      console.warn("Failed to open external link", url, err);
    }
  };

  document.addEventListener("click", onClick, true);
  document.addEventListener("auxclick", onClick, true);
  return () => {
    document.removeEventListener("click", onClick, true);
    document.removeEventListener("auxclick", onClick, true);
  };
}

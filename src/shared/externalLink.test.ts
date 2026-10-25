import { afterEach, describe, expect, it, vi } from "vitest";
import { externalHref, installExternalLinkHandler } from "./externalLink";

describe("externalHref", () => {
  it("accepts and normalizes http, https, mailto and tel", () => {
    expect(externalHref("http://example.ru/a")).toBe("http://example.ru/a");
    expect(externalHref("https://example.ru/a?b=1#c")).toBe("https://example.ru/a?b=1#c");
    expect(externalHref("HTTPS://Example.COM/a b")).toBe("https://example.com/a%20b");
    expect(externalHref("mailto:user@example.ru")).toBe("mailto:user@example.ru");
    expect(externalHref("tel:+10000000000")).toBe("tel:+10000000000");
  });

  it("rejects note links, anchors and relative paths", () => {
    expect(externalHref("notes:Входящие/2026-10-18.md")).toBeNull();
    expect(externalHref("note:foo.md")).toBeNull();
    expect(externalHref("#section")).toBeNull();
    expect(externalHref("foo.md")).toBeNull();
    expect(externalHref("./a/b.md")).toBeNull();
    expect(externalHref("../a.md")).toBeNull();
    expect(externalHref("/wd/Notes/a.md")).toBeNull();
    expect(externalHref("")).toBeNull();
    expect(externalHref("   ")).toBeNull();
  });

  it("rejects other schemes and malformed URLs", () => {
    expect(externalHref("javascript:alert(1)")).toBeNull();
    expect(externalHref("data:text/html,hi")).toBeNull();
    expect(externalHref("file:///etc/passwd")).toBeNull();
    expect(externalHref("ftp://example.ru/a")).toBeNull();
    expect(externalHref("https://")).toBeNull();
    expect(externalHref("http://[")).toBeNull();
  });

  it("normalizes against an optional base without accepting relative paths", () => {
    expect(externalHref("/a", "https://example.ru")).toBeNull();
    expect(externalHref("https://example.ru/a", "https://other.ru/")).toBe("https://example.ru/a");
  });
});

describe("installExternalLinkHandler", () => {
  let uninstall: (() => void) | undefined;

  afterEach(() => {
    uninstall?.();
    uninstall = undefined;
    document.body.innerHTML = "";
  });

  function anchor(href: string): HTMLAnchorElement {
    const a = document.createElement("a");
    a.href = href;
    a.textContent = "link";
    document.body.appendChild(a);
    return a;
  }

  function click(target: Element, button: number, type: "click" | "auxclick" = "click"): MouseEvent {
    const event = new MouseEvent(type, { bubbles: true, cancelable: true, button });
    target.dispatchEvent(event);
    return event;
  }

  it("opens an external link instead of navigating the webview", () => {
    const open = vi.fn(async () => {});
    uninstall = installExternalLinkHandler(open);
    const event = click(anchor("https://example.ru/a"), 0);

    expect(open).toHaveBeenCalledWith("https://example.ru/a");
    expect(event.defaultPrevented).toBe(true);
  });

  it("leaves note and relative markdown links to their own handlers", () => {
    const open = vi.fn(async () => {});
    uninstall = installExternalLinkHandler(open);

    const note = click(anchor("notes:Входящие/2026-10-18.md"), 0);
    const md = click(anchor("foo.md"), 0);

    expect(open).not.toHaveBeenCalled();
    expect(note.defaultPrevented).toBe(false);
    expect(md.defaultPrevented).toBe(false);
  });

  it("opens external links on middle click", () => {
    const open = vi.fn(async () => {});
    uninstall = installExternalLinkHandler(open);
    const event = click(anchor("https://example.ru/a"), 1, "auxclick");

    expect(open).toHaveBeenCalledWith("https://example.ru/a");
    expect(event.defaultPrevented).toBe(true);
  });

  it("ignores clicks that were already prevented", () => {
    const open = vi.fn(async () => {});
    const prevent = (e: MouseEvent) => e.preventDefault();
    document.addEventListener("click", prevent, true);
    uninstall = () => {
      document.removeEventListener("click", prevent, true);
      cleanup?.();
    };
    const cleanup = installExternalLinkHandler(open);

    click(anchor("https://example.ru/a"), 0);

    expect(open).not.toHaveBeenCalled();
  });

  it("swallows opener failures", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const open = vi.fn(() => Promise.reject(new Error("no handler")));
    uninstall = installExternalLinkHandler(open);

    click(anchor("https://example.ru/a"), 0);
    await Promise.resolve();

    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("stops listening after cleanup", () => {
    const open = vi.fn(async () => {});
    installExternalLinkHandler(open)();

    click(anchor("https://example.ru/a"), 0);

    expect(open).not.toHaveBeenCalled();
  });
});

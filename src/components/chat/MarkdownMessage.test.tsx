import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MarkdownMessage } from "./MarkdownMessage";
import { setNoteLinkHandler } from "../../shared/noteLink";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;
let handler: (relPath: string) => void;

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  handler = vi.fn();
  setNoteLinkHandler(handler);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  setNoteLinkHandler(null);
  vi.clearAllMocks();
});

async function render(md: string): Promise<void> {
  await act(async () => {
    root.render(<MarkdownMessage text={md} />);
  });
}

function firstAnchor(): HTMLAnchorElement {
  const a = container.querySelector(".preview-body a") as HTMLAnchorElement | null;
  if (!a) throw new Error("no anchor rendered");
  return a;
}

function click(el: Element): MouseEvent {
  const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
  act(() => {
    el.dispatchEvent(ev);
  });
  return ev;
}

describe("MarkdownMessage note-link clicks", () => {
  it("opens an explicit note: link inside Notes and prevents the default", async () => {
    await render("[Заметка](note:foo.md)");
    const ev = click(firstAnchor());
    expect(handler).toHaveBeenCalledWith("foo.md");
    expect(ev.defaultPrevented).toBe(true);
  });

  it("opens a relative .md link inside Notes and prevents the default", async () => {
    await render("[Заметка](a/b.md)");
    const ev = click(firstAnchor());
    expect(handler).toHaveBeenCalledWith("a/b.md");
    expect(ev.defaultPrevented).toBe(true);
  });

  it("leaves an http link untouched (no note handler, no preventDefault)", async () => {
    await render("[сайт](http://example.ru/a.md)");
    const ev = click(firstAnchor());
    expect(handler).not.toHaveBeenCalled();
    expect(ev.defaultPrevented).toBe(false);
  });

  it("leaves a mailto link untouched", async () => {
    await render("[почта](mailto:a@example.ru)");
    const ev = click(firstAnchor());
    expect(handler).not.toHaveBeenCalled();
    expect(ev.defaultPrevented).toBe(false);
  });

  it("does not treat a code block copy button as a note link", async () => {
    await render("```js\nconsole.log(1)\n```");
    const btn = container.querySelector("button.md-code-copy");
    expect(btn).not.toBeNull();
    click(btn as Element);
    expect(handler).not.toHaveBeenCalled();
  });
});

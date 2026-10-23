import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ChatComposer } from "./composer/ChatComposer";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

function render(over: Partial<Parameters<typeof ChatComposer>[0]> = {}) {
  return (
    <ChatComposer
      text="текст"
      onTextChange={() => {}}
      onSend={() => {}}
      onStop={() => {}}
      onClear={() => {}}
      running
      queueCount={0}
      placeholder="…"
      {...over}
    />
  );
}

describe("ChatComposer stop button", () => {
  it("reads «Стоп» while running without a pending stop", async () => {
    await act(async () => {
      root.render(render());
    });
    expect(container.querySelector(".stop-btn")?.textContent).toBe("Стоп");
  });

  it("reads «Останавливаю…» while a stop is pending and still calls onStop on click", async () => {
    const onStop = vi.fn();
    await act(async () => {
      root.render(render({ stopping: true, onStop }));
    });
    const btn = container.querySelector<HTMLButtonElement>(".stop-btn");
    expect(btn?.textContent).toBe("Останавливаю…");
    await act(async () => {
      btn?.click();
    });
    expect(onStop).toHaveBeenCalledTimes(1);
  });

  it("shows the clear button instead of stop when not running", async () => {
    await act(async () => {
      root.render(render({ running: false, stopping: true }));
    });
    expect(container.querySelector(".stop-btn")).toBeNull();
    expect(container.querySelector(".clear-btn")).not.toBeNull();
  });
});

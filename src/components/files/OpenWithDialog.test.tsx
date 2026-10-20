import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import type { FsEntry, OpenWithApp } from "../../api/types";
import OpenWithDialog from "./OpenWithDialog";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const entry: FsEntry = {
  name: "example.pdf",
  relPath: "example.pdf",
  kind: "file",
  ext: "pdf",
  size: 1,
  modified: 0,
  isMarkdown: false,
  isText: false,
  isImage: false,
};

const apps: OpenWithApp[] = [
  { name: "Example Viewer", path: "/apps/example", recommended: true },
];

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

async function click(el: Element) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

async function pressEsc() {
  await act(async () => {
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  });
}

async function mount(onClose = vi.fn()) {
  await act(async () => {
    root.render(
      <OpenWithDialog entry={entry} apps={apps} onPick={async () => {}} onClose={onClose} />,
    );
  });
  return onClose;
}

describe("OpenWithDialog close behavior", () => {
  it("closes on Escape", async () => {
    const onClose = await mount();
    await pressEsc();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("closes on backdrop click", async () => {
    const onClose = await mount();
    await click(container.querySelector<HTMLElement>(".modal-backdrop")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("does not close on content clicks", async () => {
    const onClose = await mount();
    await click(container.querySelector<HTMLElement>(".open-with-list")!);
    expect(onClose).not.toHaveBeenCalled();
  });

  it("closes on the close button", async () => {
    const onClose = await mount();
    await click(container.querySelector<HTMLElement>(".open-with-head button")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

describe("OpenWithDialog remembered app", () => {
  it("titles the chooser «в редакторе» and marks the remembered app", async () => {
    await act(async () => {
      root.render(
        <OpenWithDialog
          entry={entry}
          apps={apps}
          rememberedPath="/apps/example"
          onPick={async () => {}}
          onClose={() => {}}
        />,
      );
    });
    expect(container.querySelector(".open-with-head h3")?.textContent).toContain("в редакторе");
    const item = container.querySelector<HTMLElement>(".open-with-item")!;
    expect(item.className).toContain("remembered");
    expect(item.textContent).toContain("запомнено");
  });

  it("marks nothing when no app was remembered for the extension", async () => {
    await mount();
    const item = container.querySelector<HTMLElement>(".open-with-item")!;
    expect(item.className).not.toContain("remembered");
    expect(item.textContent).not.toContain("запомнено");
  });
});

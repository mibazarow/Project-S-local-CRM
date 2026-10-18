import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { versionList, onVersionsChanged } = vi.hoisted(() => ({
  versionList: vi.fn(async () => [] as unknown[]),
  onVersionsChanged: vi.fn(async () => () => {}),
}));

vi.mock("../../api/ipc", () => ({ versionList, onVersionsChanged }));

import VersionHistory from "./VersionHistory";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  versionList.mockReset();
  versionList.mockResolvedValue([]);
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

async function mount() {
  const onClose = vi.fn();
  await act(async () => {
    root.render(
      <VersionHistory
        scope="notes"
        absPath="/notes/example.md"
        onRestore={async () => {}}
        onClose={onClose}
      />,
    );
  });
  return onClose;
}

describe("VersionHistory close behavior", () => {
  it("closes on backdrop click", async () => {
    const onClose = await mount();
    await click(container.querySelector<HTMLElement>(".modal-backdrop")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("does not close on Escape", async () => {
    const onClose = await mount();
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("closes on the close button", async () => {
    const onClose = await mount();
    await click(container.querySelector<HTMLElement>(".settings-close")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

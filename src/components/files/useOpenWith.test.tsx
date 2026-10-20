import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { FsEntry, OpenWithApp } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const listOpenWithApps = vi.hoisted(
  () => vi.fn(async (_ext: string): Promise<OpenWithApp[]> => []),
);
const getOpenWith = vi.hoisted(
  () => vi.fn(async (_ext: string): Promise<OpenWithApp | null> => null),
);
const openWith = vi.hoisted(
  () => vi.fn(async () => ({ opened: true, app: null as string | null })),
);
const rememberOpenWith = vi.hoisted(() => vi.fn(async () => undefined));
const fsOpenExternal = vi.hoisted(() => vi.fn(async () => undefined));

vi.mock("../../api/ipc", () => ({
  listOpenWithApps,
  getOpenWith,
  openWith,
  rememberOpenWith,
  fsOpenExternal,
}));

import { useOpenWith } from "./useOpenWith";

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

const app: OpenWithApp = { name: "Example Viewer", path: "/apps/example", recommended: true };

function Probe() {
  const ow = useOpenWith(1);
  return (
    <>
      {ow.error && <div className="ow-error">{ow.error}</div>}
      <button onClick={() => void ow.showChooser(entry)}>chooser</button>
      {ow.dialog}
    </>
  );
}

let container: HTMLDivElement;
let root: Root;

const settle = async () => {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
};

const click = async (el: Element) => {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
  await settle();
};

beforeEach(() => {
  listOpenWithApps.mockReset().mockResolvedValue([]);
  getOpenWith.mockReset().mockResolvedValue(null);
  openWith.mockReset().mockResolvedValue({ opened: true, app: null });
  rememberOpenWith.mockReset().mockResolvedValue(undefined);
  fsOpenExternal.mockReset().mockResolvedValue(undefined);
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  act(() => {
    root.render(<Probe />);
  });
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("useOpenWith chooser flow", () => {
  it("always shows the chooser and marks the remembered app for the extension", async () => {
    listOpenWithApps.mockResolvedValue([app]);
    getOpenWith.mockResolvedValue({ ...app, recommended: false });
    await click(container.querySelector("button")!);
    expect(listOpenWithApps).toHaveBeenCalledWith("pdf");
    expect(getOpenWith).toHaveBeenCalledWith("pdf");
    const item = container.querySelector<HTMLElement>(".open-with-item")!;
    expect(item.className).toContain("remembered");
    expect(item.textContent).toContain("запомнено");
  });

  it("picking an app opens it; with «Запомнить выбор» it is stored for the extension", async () => {
    listOpenWithApps.mockResolvedValue([app]);
    await click(container.querySelector("button")!);
    const checkbox = container.querySelector<HTMLInputElement>(".ow-remember input")!;
    await act(async () => {
      checkbox.click();
    });
    await click(container.querySelector<HTMLElement>(".open-with-item")!);
    expect(openWith).toHaveBeenCalledWith("project", "example.pdf", 1, "/apps/example");
    expect(rememberOpenWith).toHaveBeenCalledWith("pdf", "/apps/example");
    expect(container.querySelector(".open-with-modal")).toBeNull();
  });

  it("with no compatible apps the chooser falls back to the system default", async () => {
    await click(container.querySelector("button")!);
    expect(container.querySelector(".open-with-empty")).not.toBeNull();
    const fallback = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
      (b) => b.textContent === "Открыть по умолчанию",
    )!;
    await click(fallback);
    expect(fsOpenExternal).toHaveBeenCalledWith(1, "example.pdf");
    expect(container.querySelector(".open-with-modal")).toBeNull();
  });
});

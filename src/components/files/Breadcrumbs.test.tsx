import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

import Breadcrumbs from "./Breadcrumbs";

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

async function mount(path: string, onNavigate = vi.fn()) {
  await act(async () => {
    root.render(<Breadcrumbs path={path} onNavigate={onNavigate} />);
  });
  return onNavigate;
}

const click = (el: Element) =>
  act(() => el.dispatchEvent(new MouseEvent("click", { bubbles: true })));

const crumbs = () => Array.from(container.querySelectorAll(".crumb"));
const links = () => Array.from(container.querySelectorAll("button.crumb"));
const label = (el: Element) => el.textContent ?? "";

describe("Breadcrumbs home crumb (Q.md item 9)", () => {
  it("renders the home crumb even at the project root, as a non-clickable span", async () => {
    await mount("");
    expect(container.querySelector(".crumbs")).not.toBeNull();
    const home = container.querySelector(".crumb-home")!;
    expect(home.tagName).toBe("SPAN");
    expect(home.querySelector("svg")).not.toBeNull();
    expect(links()).toHaveLength(0);
  });

  it("the home crumb is the first crumb and navigates to the root", async () => {
    const onNavigate = await mount("docs/sub");
    expect(crumbs()[0]).toBe(container.querySelector(".crumb-home"));
    await click(container.querySelector("button.crumb-home")!);
    expect(onNavigate).toHaveBeenCalledWith("");
  });

  it("intermediate segments are links, the current segment is not", async () => {
    const onNavigate = await mount("docs/sub");
    expect(crumbs().map(label)).toEqual(["", "docs", "sub"]);
    expect(links().map(label)).toEqual(["", "docs"]);
    const current = crumbs()[2];
    expect(current.tagName).toBe("SPAN");
    await click(links()[1]);
    expect(onNavigate).toHaveBeenCalledWith("docs");
  });

  it("a file path in list view: folder crumbs clickable, the file segment is current", async () => {
    const onNavigate = await mount("docs/a.txt");
    expect(crumbs().map(label)).toEqual(["", "docs", "a.txt"]);
    expect(links().map(label)).toEqual(["", "docs"]);
    await click(links()[1]);
    expect(onNavigate).toHaveBeenCalledWith("docs");
  });
});

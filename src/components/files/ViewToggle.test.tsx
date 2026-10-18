import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import ViewToggle from "./ViewToggle";

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

async function click(el: Element) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

const buttons = () => Array.from(container.querySelectorAll("button"));

describe("ViewToggle", () => {
  it("renders the view-toggle wrapper with its title and vt buttons", async () => {
    await act(async () => {
      root.render(<ViewToggle view="list" onChange={() => {}} />);
    });
    const wrapper = container.querySelector(".view-toggle")!;
    expect(wrapper.getAttribute("title")).toBe("Вид файлов");
    expect(buttons()).toHaveLength(2);
    expect(buttons()[0].className).toBe("vt active");
    expect(buttons()[0].getAttribute("title")).toBe("Список");
    expect(buttons()[1].className).toBe("vt");
    expect(buttons()[1].getAttribute("title")).toBe("Сетка");
    expect(buttons()[0].querySelector("svg")).not.toBeNull();
    expect(buttons()[1].querySelector("svg")).not.toBeNull();
  });

  it("clicking the grid button calls onChange with grid", async () => {
    const onChange = vi.fn();
    await act(async () => {
      root.render(<ViewToggle view="list" onChange={onChange} />);
    });
    await click(buttons()[1]);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith("grid");
  });
});

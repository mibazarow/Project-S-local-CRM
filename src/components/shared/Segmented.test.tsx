import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import Segmented from "./Segmented";

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

async function render(el: React.ReactElement) {
  await act(async () => {
    root.render(el);
  });
}

async function click(el: Element) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

const buttons = () =>
  Array.from(container.querySelectorAll("button"));

const TABS = [
  { value: "files", label: "Файлы" },
  { value: "tasks", label: "Задачи" },
] as const;

describe("Segmented", () => {
  it("renders wrapper and seg buttons with classes, order and labels", async () => {
    await render(
      <Segmented value="files" options={TABS} onChange={() => {}} />,
    );
    const wrapper = container.querySelector(".segmented")!;
    expect(wrapper.tagName).toBe("DIV");
    expect(buttons()).toHaveLength(2);
    expect(buttons()[0].className).toBe("seg active");
    expect(buttons()[0].textContent).toBe("Файлы");
    expect(buttons()[1].className).toBe("seg");
    expect(buttons()[1].textContent).toBe("Задачи");
    expect(wrapper.firstElementChild).toBe(buttons()[0]);
    expect(wrapper.children[1]).toBe(buttons()[1]);
  });

  it("marks only the active option", async () => {
    await render(
      <Segmented value="tasks" options={TABS} onChange={() => {}} />,
    );
    expect(buttons()[0].className).toBe("seg");
    expect(buttons()[1].className).toBe("seg active");
  });

  it("clicking a non-active button calls onChange with its value", async () => {
    const onChange = vi.fn();
    await render(<Segmented value="files" options={TABS} onChange={onChange} />);
    await click(buttons()[1]);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith("tasks");
  });

  it("clicking the active button still calls onChange, matching the old handlers", async () => {
    const onChange = vi.fn();
    await render(<Segmented value="files" options={TABS} onChange={onChange} />);
    await click(buttons()[0]);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith("files");
  });

  it("disabled options render disabled and ignore clicks", async () => {
    const onChange = vi.fn();
    await render(
      <Segmented
        value="files"
        options={[
          { value: "files", label: "Файлы" },
          { value: "tasks", label: "Задачи", disabled: true },
        ]}
        onChange={onChange}
      />,
    );
    expect(buttons()[1].disabled).toBe(true);
    await click(buttons()[1]);
    expect(onChange).not.toHaveBeenCalled();
  });

  it("renders icons instead of labels and per-button titles", async () => {
    await render(
      <Segmented
        value="list"
        options={[
          { value: "list", icon: <svg data-icon="list" />, title: "Список" },
          { value: "grid", icon: <svg data-icon="grid" />, title: "Сетка" },
        ]}
        onChange={() => {}}
      />,
    );
    expect(buttons()[0].querySelector("svg[data-icon='list']")).not.toBeNull();
    expect(buttons()[1].querySelector("svg[data-icon='grid']")).not.toBeNull();
    expect(buttons()[0].getAttribute("title")).toBe("Список");
    expect(buttons()[1].getAttribute("title")).toBe("Сетка");
    expect(buttons()[0].textContent).toBe("");
  });

  it("supports custom wrapper/button classes, wrapper title and option classNames", async () => {
    await render(
      <Segmented
        value="b"
        options={[
          { value: "a", label: "A", className: "extra" },
          { value: "b", label: "B" },
        ]}
        onChange={() => {}}
        wrapperClassName="view-toggle"
        buttonClassName="vt"
        title="Вид файлов"
      />,
    );
    const wrapper = container.querySelector(".view-toggle")!;
    expect(wrapper.getAttribute("title")).toBe("Вид файлов");
    expect(buttons()[0].className).toBe("vt extra");
    expect(buttons()[1].className).toBe("vt active");
  });
});

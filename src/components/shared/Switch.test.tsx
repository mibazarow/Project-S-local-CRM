import { act, createRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import Switch from "./Switch";

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

const label = () => container.querySelector("label")!;
const input = () => container.querySelector<HTMLInputElement>("input")!;

describe("Switch", () => {
  it("renders the markup it replaces: label.switch > input + span.track", async () => {
    await render(<Switch checked={false} onChange={() => {}} />);
    expect(label().className).toBe("switch");
    expect(input().type).toBe("checkbox");
    expect(label().querySelector("span")!.className).toBe("track");
    expect(label().children.length).toBe(2);
  });

  it("keeps the checked state and reports the new value on change", async () => {
    const seen: boolean[] = [];
    await render(
      <Switch checked onChange={(e) => seen.push(e.target.checked)} />,
    );
    expect(input().checked).toBe(true);
    act(() => {
      input().click();
    });
    expect(seen).toEqual([false]);
    expect(input().checked).toBe(true);
  });

  it("forwards disabled, id and aria-label to the input", async () => {
    const onChange = vi.fn();
    await render(
      <Switch id="autostart" aria-label="Запускаться с системой" checked={false} disabled onChange={onChange} />,
    );
    expect(input().disabled).toBe(true);
    expect(input().id).toBe("autostart");
    expect(input().getAttribute("aria-label")).toBe("Запускаться с системой");
    act(() => {
      input().click();
    });
    expect(onChange).not.toHaveBeenCalled();
  });

  it("puts labelProps on the label and keeps the switch class", async () => {
    await render(
      <Switch
        labelProps={{ title: "Поставить на паузу" }}
        checked
        onChange={() => {}}
      />,
    );
    expect(label().getAttribute("title")).toBe("Поставить на паузу");
    expect(label().className).toBe("switch");
  });

  it("stops a click on the track from reaching the row", async () => {
    const onRow = vi.fn();
    const onToggle = vi.fn();
    await render(
      <div onClick={onRow}>
        <Switch
          labelProps={{
            onClick: (e) => e.stopPropagation(),
          }}
          checked={false}
          onChange={onToggle}
        />
      </div>,
    );
    act(() => {
      label().querySelector("span")!.click();
    });
    expect(onRow).not.toHaveBeenCalled();
    expect(onToggle).toHaveBeenCalledTimes(1);
  });

  it("forwards a ref to the input", async () => {
    const ref = createRef<HTMLInputElement>();
    await render(<Switch ref={ref} checked={false} onChange={() => {}} />);
    expect(ref.current).toBeInstanceOf(HTMLInputElement);
  });
});

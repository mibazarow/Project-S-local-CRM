import { act, createRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import Badge from "./Badge";
import Button from "./Button";
import IconButton from "./IconButton";

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

function click(el: Element) {
  act(() => {
    (el as HTMLElement).click();
  });
}

const btn = () => container.querySelector("button")!;
const span = () => container.querySelector("span")!;

describe("Button", () => {
  it("maps variants to the existing classes", async () => {
    await render(<Button variant="primary">Save</Button>);
    expect(btn().className).toBe("primary-btn");
    await render(<Button variant="ghost">Cancel</Button>);
    expect(btn().className).toBe("ghost-btn");
    await render(<Button variant="mini">More</Button>);
    expect(btn().className).toBe("mini-btn");
  });

  it("keeps the class order of the markup it replaces", async () => {
    await render(
      <Button variant="ghost" size="small" className="danger">
        Delete
      </Button>,
    );
    expect(btn().className).toBe("ghost-btn small danger");
  });

  it("omits the class attribute when there is nothing to compose", async () => {
    await render(<Button onClick={() => {}}>Plain</Button>);
    expect(btn().hasAttribute("class")).toBe(false);
  });

  it("passes type through instead of defaulting it", async () => {
    await render(
      <form>
        <Button type="submit">Submit</Button>
      </form>,
    );
    expect(btn().getAttribute("type")).toBe("submit");
    await render(<Button>Implicit</Button>);
    expect(btn().hasAttribute("type")).toBe(false);
  });

  it("is disabled and ignores clicks while disabled", async () => {
    const onClick = vi.fn();
    await render(
      <Button variant="primary" disabled onClick={onClick}>
        Save
      </Button>,
    );
    expect(btn().disabled).toBe(true);
    click(btn());
    expect(onClick).not.toHaveBeenCalled();
  });

  it("calls onClick when enabled", async () => {
    const onClick = vi.fn();
    await render(
      <Button variant="primary" onClick={onClick}>
        Save
      </Button>,
    );
    click(btn());
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("forwards keyboard handlers and refs", async () => {
    const onKeyDown = vi.fn();
    const ref = createRef<HTMLButtonElement>();
    await render(<Button ref={ref} onKeyDown={onKeyDown}>Key</Button>);
    expect(ref.current).toBeInstanceOf(HTMLButtonElement);
    act(() => {
      ref.current!.focus();
      ref.current!.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    });
    expect(document.activeElement).toBe(ref.current);
    expect(onKeyDown).toHaveBeenCalledTimes(1);
  });
});

describe("IconButton", () => {
  it("renders the icon-btn class and keeps extra classes", async () => {
    await render(
      <IconButton size="small" className="settings-close" title="Закрыть" onClick={() => {}}>
        ×
      </IconButton>,
    );
    expect(btn().className).toBe("icon-btn small settings-close");
    expect(btn().getAttribute("title")).toBe("Закрыть");
  });

  it("exposes an accessible name via aria-label", async () => {
    await render(<IconButton aria-label="Настройки">⚙</IconButton>);
    expect(btn().getAttribute("aria-label")).toBe("Настройки");
  });

  it("is focusable by keyboard and fires onClick", async () => {
    const onClick = vi.fn();
    await render(
      <IconButton title="Поиск" onClick={onClick}>
        ⌕
      </IconButton>,
    );
    act(() => {
      btn().focus();
    });
    expect(document.activeElement).toBe(btn());
    click(btn());
    expect(onClick).toHaveBeenCalledTimes(1);
  });
});

describe("Badge", () => {
  it("renders the badge class and tone modifiers", async () => {
    await render(<Badge>file</Badge>);
    expect(span().className).toBe("badge");
    await render(<Badge tone="active">активен</Badge>);
    expect(span().className).toBe("badge st-active");
  });

  it("composes an extra className after the tone", async () => {
    await render(
      <Badge tone="disabled" className="mcp-state">
        off
      </Badge>,
    );
    expect(span().className).toBe("badge st-disabled mcp-state");
  });
});

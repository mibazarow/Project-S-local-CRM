import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useDebouncedValue } from "./useDebouncedValue";

// React 19 refuses to run act() unless the act environment flag is set.
(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const DELAY = 250;

function Probe({ value }: { value: string }) {
  const debounced = useDebouncedValue(value, DELAY);
  return <span>{debounced}</span>;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.useFakeTimers();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

const text = () => container.textContent;

describe("useDebouncedValue", () => {
  it("shows the initial value immediately", () => {
    act(() => root.render(<Probe value="a" />));
    expect(text()).toBe("a");
  });

  it("applies a new value only after the delay", () => {
    act(() => root.render(<Probe value="a" />));
    act(() => root.render(<Probe value="b" />));
    expect(text()).toBe("a");
    act(() => vi.advanceTimersByTime(DELAY - 1));
    expect(text()).toBe("a");
    act(() => vi.advanceTimersByTime(1));
    expect(text()).toBe("b");
  });

  it("resets the timer on rapid changes so only the last value lands", () => {
    act(() => root.render(<Probe value="a" />));
    act(() => root.render(<Probe value="b" />));
    act(() => vi.advanceTimersByTime(100));
    act(() => root.render(<Probe value="c" />));
    act(() => vi.advanceTimersByTime(100));
    expect(text()).toBe("a");
    act(() => vi.advanceTimersByTime(DELAY - 100));
    expect(text()).toBe("c");
  });

  it("clears the pending timer on unmount", () => {
    act(() => root.render(<Probe value="a" />));
    act(() => root.render(<Probe value="b" />));
    expect(vi.getTimerCount()).toBeGreaterThan(0);
    act(() => root.unmount());
    expect(vi.getTimerCount()).toBe(0);
  });
});

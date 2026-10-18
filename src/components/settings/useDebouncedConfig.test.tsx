import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../../api/ipc", () => ({
  readConfig: vi.fn(),
  writeConfig: vi.fn(),
}));

import * as ipc from "../../api/ipc";
import { useDebouncedConfig, type DebouncedConfig } from "./useDebouncedConfig";

// React 19 refuses to run act() unless the act environment flag is set.
(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

interface ProbeForm {
  a: string;
  b: string;
}

// Raw file with a key the form does not own; it must survive every write.
const RAW = { a: "1", b: "2", staleKey: { legacy: true } };

const toForm = (raw: Record<string, unknown>): ProbeForm => ({
  a: String(raw.a ?? ""),
  b: String(raw.b ?? ""),
});
const toPatch = (f: ProbeForm, raw: Record<string, unknown>) => ({ ...raw, a: f.a, b: f.b });

const onError = vi.fn();

let hook: DebouncedConfig<ProbeForm> | null = null;

function Probe() {
  hook = useDebouncedConfig<ProbeForm>("indexing", toForm, toPatch, onError);
  return <span>{hook.form ? `${hook.form.a}|${hook.form.b}` : "null"}</span>;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  vi.useFakeTimers();
  hook = null;
  onError.mockClear();
  vi.mocked(ipc.readConfig).mockReset().mockResolvedValue({ ...RAW });
  vi.mocked(ipc.writeConfig).mockReset().mockResolvedValue(undefined);
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

// Mount and let the initial readConfig promise settle.
async function mount() {
  await act(async () => {
    root.render(<Probe />);
  });
}

const writes = () => vi.mocked(ipc.writeConfig).mock.calls;

describe("useDebouncedConfig", () => {
  it("reads the config on mount", async () => {
    await mount();
    expect(ipc.readConfig).toHaveBeenCalledWith("indexing");
    expect(container.textContent).toBe("1|2");
  });

  it("debounces writes by 500 ms and preserves unknown keys", async () => {
    await mount();
    act(() => hook!.updateForm({ a: "9" }));
    expect(writes()).toHaveLength(0);
    act(() => vi.advanceTimersByTime(499));
    expect(writes()).toHaveLength(0);
    act(() => vi.advanceTimersByTime(1));
    expect(writes()).toHaveLength(1);
    expect(writes()[0][0]).toBe("indexing");
    expect(writes()[0][1]).toEqual({ a: "9", b: "2", staleKey: { legacy: true } });
  });

  it("blur flushes the pending edit immediately and cancels the timer", async () => {
    await mount();
    act(() => hook!.updateForm({ a: "9" }));
    act(() => hook!.blurField());
    expect(writes()).toHaveLength(1);
    act(() => vi.advanceTimersByTime(500));
    expect(writes()).toHaveLength(1);
  });

  it("reload keeps the focused field's in-progress value and updates the rest", async () => {
    await mount();
    act(() => hook!.focusField("a")());
    act(() => hook!.updateForm({ a: "20." }));
    vi.mocked(ipc.readConfig).mockResolvedValue({ a: "ext", b: "ext2", staleKey: 1 });
    act(() => hook!.reload());
    await act(async () => {});
    expect(container.textContent).toBe("20.|ext2");
  });

  it("ignores edits before the config has loaded (no write, no form)", async () => {
    vi.mocked(ipc.readConfig).mockReturnValue(new Promise(() => {}));
    act(() => root.render(<Probe />));
    expect(container.textContent).toBe("null");
    act(() => hook!.updateForm({ a: "x" }));
    act(() => hook!.blurField());
    act(() => vi.advanceTimersByTime(10_000));
    expect(writes()).toHaveLength(0);
  });

  it("flushes a pending edit on unmount", async () => {
    await mount();
    act(() => hook!.updateForm({ b: "7" }));
    act(() => vi.advanceTimersByTime(100));
    act(() => root.unmount());
    expect(writes()).toHaveLength(1);
    expect(writes()[0][1]).toEqual({ a: "1", b: "7", staleKey: { legacy: true } });
  });

  it("reports write failures through onError with the message", async () => {
    await mount();
    vi.mocked(ipc.writeConfig).mockRejectedValue(new Error("disk full"));
    act(() => hook!.blurField());
    await act(async () => {});
    expect(onError).toHaveBeenCalledWith("disk full");
  });
});

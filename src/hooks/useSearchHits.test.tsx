import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSearchHits, type Options } from "./useSearchHits";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const deferred = () => {
  let resolve!: (v: string[]) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<string[]>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
};

function Probe({
  run,
  deps,
  opts,
}: {
  run: () => Promise<string[]>;
  deps: unknown[];
  opts?: Options;
}) {
  const { hits, loading, error } = useSearchHits(run, deps, opts);
  return (
    <span>
      {loading ? "L" : "-"}
      {error !== null ? `E(${error})` : ""}
      [{hits.join("|")}]
    </span>
  );
}

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

const text = () => container.textContent;

describe("useSearchHits", () => {
  it("starts loading, then shows the fetched hits", async () => {
    const d = deferred();
    act(() => root.render(<Probe run={() => d.promise} deps={["q"]} />));
    expect(text()).toBe("L[]");
    await act(async () => d.resolve(["a", "b"]));
    expect(text()).toBe("-[a|b]");
  });

  it("an empty backend result yields empty hits without an error", async () => {
    await act(async () => {
      root.render(<Probe run={async () => []} deps={["q"]} />);
    });
    expect(text()).toBe("-[]");
  });

  it("refetches when deps change", async () => {
    const run = vi.fn(async () => ["one"]);
    await act(async () => {
      root.render(<Probe run={run} deps={[1]} />);
    });
    await act(async () => {
      root.render(<Probe run={run} deps={[2]} />);
    });
    expect(run).toHaveBeenCalledTimes(2);
    expect(text()).toBe("-[one]");
  });

  it("ignores a stale response that lands after a newer one", async () => {
    const d1 = deferred();
    const d2 = deferred();
    const run = vi.fn().mockReturnValueOnce(d1.promise).mockReturnValueOnce(d2.promise);
    await act(async () => {
      root.render(<Probe run={run} deps={["q1"]} />);
    });
    act(() => root.render(<Probe run={run} deps={["q2"]} />));
    await act(async () => d2.resolve(["new"]));
    await act(async () => d1.resolve(["old"]));
    expect(text()).toBe("-[new]");
  });

  it("surfaces the backend message on rejection by default", async () => {
    await act(async () => {
      root.render(<Probe run={async () => Promise.reject({ message: "Индекс сломан" })} deps={["q"]} />);
    });
    expect(text()).toBe("-E(Индекс сломан)[]");
  });

  it("onError emptyHits falls back to empty hits with no error", async () => {
    const run = vi.fn(async () => ["one"]);
    await act(async () => {
      root.render(<Probe run={run} deps={[1]} opts={{ onError: "emptyHits" }} />);
    });
    expect(text()).toBe("-[one]");
    const failing = vi.fn(async () => {
      throw { message: "boom" };
    });
    await act(async () => {
      root.render(<Probe run={failing} deps={[2]} opts={{ onError: "emptyHits" }} />);
    });
    expect(text()).toBe("-[]");
  });

  it("skip reset clears hits and loading without calling run", async () => {
    const run = vi.fn(async () => ["one"]);
    await act(async () => {
      root.render(<Probe run={run} deps={["ab"]} />);
    });
    expect(text()).toBe("-[one]");
    await act(async () => {
      root.render(<Probe run={run} deps={[""]} opts={{ skip: true }} />);
    });
    expect(text()).toBe("-[]");
    expect(run).toHaveBeenCalledTimes(1);
  });

  it("skip keep preserves previous hits without calling run", async () => {
    const run = vi.fn(async () => ["one"]);
    await act(async () => {
      root.render(<Probe run={run} deps={["ab"]} />);
    });
    await act(async () => {
      root.render(<Probe run={run} deps={[""]} opts={{ skip: true, onSkip: "keep" }} />);
    });
    expect(text()).toBe("-[one]");
    expect(run).toHaveBeenCalledTimes(1);
  });
});

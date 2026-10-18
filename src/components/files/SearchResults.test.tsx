import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../../api/ipc", () => ({
  searchProject: vi.fn(async () => []),
  fsListDir: vi.fn(async () => []),
}));

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { renderToStaticMarkup } from "react-dom/server";
import * as ipc from "../../api/ipc";
import type { SearchHit } from "../../api/types";
import SearchResults, { renderSnippet } from "./SearchResults";

const html = (snippet: string) => renderToStaticMarkup(renderSnippet(snippet));

describe("renderSnippet", () => {
  it("turns FTS5 <<match>> markers into <mark> elements", () => {
    expect(html("a <<b>> c")).toBe("a <mark>b</mark> c");
    expect(html("<<hit>>")).toBe("<mark>hit</mark>");
  });

  it("keeps text without markers as-is", () => {
    expect(html("plain")).toBe("plain");
    expect(html("")).toBe("");
  });

  it("handles adjacent and empty markers", () => {
    expect(html("<<a>><<b>>")).toBe("<mark>a</mark><mark>b</mark>");
    expect(html("<<>>")).toBe("<mark></mark>");
  });

  it("marker content stops at the first > and is escaped on render", () => {
    expect(html("<<a<b>>")).toBe("<mark>a&lt;b</mark>");
    expect(html("x >> y <<m>> z")).toBe("x &gt;&gt; y <mark>m</mark> z");
  });

  it("returns plain strings around keyed mark elements", () => {
    const nodes = renderSnippet("a <<b>>");
    expect(nodes[0]).toBe("a ");
    const mark = nodes[1] as { type: string; key: string | null; props: { children: string } };
    expect(mark.type).toBe("mark");
    expect(mark.key).toBe("2");
    expect(mark.props.children).toBe("b");
    expect(nodes[2]).toBe("");
  });
});

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const hit = (over: Partial<SearchHit>): SearchHit => ({
  relPath: "a.md",
  name: "a.md",
  kind: "file",
  score: 1,
  snippet: "",
  matchTypes: ["fts"],
  modified: 0,
  ...over,
});

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
  vi.clearAllMocks();
});

const renderResults = async () => {
  await act(async () => {
    root.render(
      <SearchResults projectId={1} query="q" onSelect={vi.fn()} onOpenTask={vi.fn()} />,
    );
  });
};

describe("SearchResults rendering", () => {
  it("shows the loading note while the backend request is pending", async () => {
    vi.mocked(ipc.searchProject).mockReturnValue(new Promise<SearchHit[]>(() => {}));
    await renderResults();
    expect(container.textContent).toBe("Поиск…");
  });

  it("shows only the empty note when the backend returns no hits", async () => {
    vi.mocked(ipc.searchProject).mockResolvedValue([]);
    await renderResults();
    expect(container.textContent).toBe("Ничего не найдено");
  });

  it("shows the backend error message when the request rejects", async () => {
    vi.mocked(ipc.searchProject).mockRejectedValue({ code: "x", message: "Индекс сломан" });
    await renderResults();
    expect(container.querySelector(".tree-error")?.textContent).toBe("Индекс сломан");
  });

  it("renders hits in backend order without rescoring", async () => {
    vi.mocked(ipc.searchProject).mockResolvedValue([
      hit({ relPath: "low.md", name: "low.md", score: 1 }),
      hit({ relPath: "high.md", name: "high.md", score: 99 }),
    ]);
    await renderResults();
    const names = [...container.querySelectorAll(".search-hit-name")].map((n) => n.textContent);
    expect(names).toEqual(["low.md", "high.md"]);
  });
});

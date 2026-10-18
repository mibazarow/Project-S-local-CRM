import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import { renderToStaticMarkup } from "react-dom/server";
import { renderSnippet } from "./SearchResults";

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

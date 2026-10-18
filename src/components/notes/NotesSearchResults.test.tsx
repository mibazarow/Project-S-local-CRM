import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../../api/ipc", () => ({
  searchNotes: vi.fn(async () => []),
  notesList: vi.fn(async () => []),
}));

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import * as ipc from "../../api/ipc";
import type { SearchHit } from "../../api/types";
import NotesSearchResults from "./NotesSearchResults";

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
    root.render(<NotesSearchResults query="q" treeVersion={0} onOpen={vi.fn()} />);
  });
};

describe("NotesSearchResults rendering", () => {
  it("shows the loading note while the backend request is pending", async () => {
    vi.mocked(ipc.searchNotes).mockReturnValue(new Promise<SearchHit[]>(() => {}));
    await renderResults();
    expect(container.querySelector(".notes-search-results")?.textContent).toBe("Поиск…");
  });

  it("shows only the empty note when the backend returns no hits", async () => {
    vi.mocked(ipc.searchNotes).mockResolvedValue([]);
    await renderResults();
    expect(container.textContent).toBe("Ничего не найдено");
  });

  it("shows the backend error message when the request rejects", async () => {
    vi.mocked(ipc.searchNotes).mockRejectedValue({ code: "x", message: "Индекс сломан" });
    await renderResults();
    expect(container.querySelector(".tree-error")?.textContent).toBe("Индекс сломан");
  });

  it("renders hits in backend order without rescoring", async () => {
    vi.mocked(ipc.searchNotes).mockResolvedValue([
      hit({ relPath: "low.md", name: "low.md", score: 1 }),
      hit({ relPath: "high.md", name: "high.md", score: 99 }),
    ]);
    await renderResults();
    const names = [...container.querySelectorAll(".search-hit-name")].map((n) => n.textContent);
    expect(names).toEqual(["low.md", "high.md"]);
  });
});

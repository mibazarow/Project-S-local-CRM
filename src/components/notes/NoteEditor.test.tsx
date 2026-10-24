import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EditorView } from "@codemirror/view";
import type { FsEntry } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const notesRead = vi.hoisted(() => vi.fn(async (_rel: string) => ""));
const notesReadBinaryDataUrl = vi.hoisted(
  () => vi.fn(async (_rel: string, _maxBytes: number) => ""),
);
const notesWrite = vi.hoisted(
  () => vi.fn(async (_rel: string, _content: string) => ({})),
);
const notesEnsure = vi.hoisted(() => vi.fn(async () => "/wd/Notes"));
const openWith = vi.hoisted(
  () => vi.fn(async (..._args: unknown[]) => ({ opened: true, app: null })),
);

// Event bus: the editor subscribes to notes-changed / index-changed; tests
// fire those handlers to simulate an external write to the open note.
const bus = vi.hoisted(() => ({
  notes: [] as ((p: { relPath: string }) => void)[],
  index: [] as ((p: { projectId: number }) => void)[],
}));
const onNotesChanged = vi.hoisted(
  () =>
    vi.fn(async (h: (p: { relPath: string }) => void) => {
      bus.notes.push(h);
      return () => {
        bus.notes = bus.notes.filter((x) => x !== h);
      };
    }),
);
const onIndexChanged = vi.hoisted(
  () =>
    vi.fn(async (h: (p: { projectId: number }) => void) => {
      bus.index.push(h);
      return () => {
        bus.index = bus.index.filter((x) => x !== h);
      };
    }),
);

vi.mock("../../api/ipc", () => ({
  notesRead,
  notesReadBinaryDataUrl,
  notesWrite,
  notesEnsure,
  openWith,
  onNotesChanged,
  onIndexChanged,
}));

import NoteEditor from "./NoteEditor";

const entry = (over: Partial<FsEntry> & { name: string; relPath: string }): FsEntry => ({
  kind: "file",
  ext: "",
  size: 10,
  modified: 0,
  isMarkdown: false,
  isText: false,
  isImage: false,
  ...over,
});

const mdEntry = entry({ name: "note.md", relPath: "note.md", ext: "md", isMarkdown: true, isText: true });
const txtEntry = entry({ name: "plan.txt", relPath: "plan.txt", ext: "txt", isText: true });
const pngEntry = entry({ name: "pic.png", relPath: "pic.png", ext: "png", isImage: true, size: 100 });
const pdfEntry = entry({ name: "doc.pdf", relPath: "doc.pdf", ext: "pdf", size: 1000 });
const zipEntry = entry({ name: "archive.zip", relPath: "archive.zip", ext: "zip", size: 2048 });

let container: HTMLDivElement;
let root: Root;

const settle = async () => {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
};

const mount = async (entry: FsEntry | null) => {
  await act(async () => {
    root.render(<NoteEditor entry={entry} onDirtyChange={() => {}} />);
  });
  await settle();
};

const button = (label: string) =>
  Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
    (b) => b.textContent === label,
  );

beforeEach(() => {
  notesRead.mockReset().mockResolvedValue("");
  notesReadBinaryDataUrl.mockReset().mockResolvedValue("");
  notesWrite.mockReset().mockResolvedValue({});
  notesEnsure.mockReset().mockResolvedValue("/wd/Notes");
  openWith.mockReset().mockResolvedValue({ opened: true, app: null });
  bus.notes = [];
  bus.index = [];
  onNotesChanged.mockReset().mockImplementation(async (h: (p: { relPath: string }) => void) => {
    bus.notes.push(h);
    return () => {
      bus.notes = bus.notes.filter((x) => x !== h);
    };
  });
  onIndexChanged.mockReset().mockImplementation(async (h: (p: { projectId: number }) => void) => {
    bus.index.push(h);
    return () => {
      bus.index = bus.index.filter((x) => x !== h);
    };
  });
  URL.createObjectURL = vi.fn(() => "blob:pdf-preview");
  URL.revokeObjectURL = vi.fn();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("NoteEditor empty state", () => {
  it("shows the renamed section placeholder", async () => {
    await mount(null);
    expect(container.querySelector(".placeholder h1")?.textContent).toBe("Заметки и файлы");
    expect(container.textContent).not.toContain("Блокнот");
  });
});

describe("NoteEditor markdown editor", () => {
  it("renders markdown blocks through the block editor", async () => {
    notesRead.mockResolvedValue("# Заголовок\n\nТекст заметки");
    await mount(mdEntry);
    expect(notesRead).toHaveBeenCalledWith("note.md");
    expect(container.querySelector(".md-block h1")?.textContent).toBe("Заголовок");
    expect(container.querySelector(".md-block p")?.textContent).toBe("Текст заметки");
    expect(button("Исходник")).toBeTruthy();
    expect(container.querySelector('[title="Полужирный"]')).toBeTruthy();
  });

  it("keeps autosave writing edited markdown via notes_write", async () => {
    notesRead.mockResolvedValue("первый блок");
    await mount(mdEntry);
    const block = container.querySelector<HTMLElement>(".md-block")!;
    await act(async () => {
      block.click();
    });
    const ta = container.querySelector<HTMLTextAreaElement>(".md-block-textarea")!;
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLTextAreaElement.prototype,
      "value",
    )!.set!;
    vi.useFakeTimers();
    act(() => {
      setter.call(ta, "изменённый блок");
      ta.dispatchEvent(new Event("input", { bubbles: true }));
      ta.blur();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(900);
    });
    vi.useRealTimers();
    expect(notesWrite).toHaveBeenCalledWith("note.md", "изменённый блок");
  });

  it("preserves blank-line structure when committing an edited block", async () => {
    notesRead.mockResolvedValue("первый\n\n\n\nвторой\n\n");
    await mount(mdEntry);
    const blocks = container.querySelectorAll<HTMLElement>(".md-block");
    await act(async () => {
      blocks[1].click();
    });
    const ta = container.querySelector<HTMLTextAreaElement>(".md-block-textarea")!;
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLTextAreaElement.prototype,
      "value",
    )!.set!;
    vi.useFakeTimers();
    act(() => {
      setter.call(ta, "изменённый");
      ta.dispatchEvent(new Event("input", { bubbles: true }));
      ta.blur();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(900);
    });
    vi.useRealTimers();
    expect(notesWrite).toHaveBeenCalledWith("note.md", "первый\n\n\n\nизменённый\n\n");
  });
});

describe("NoteEditor non-md previews", () => {
  it("renders text files read-only through notes_read", async () => {
    notesRead.mockResolvedValue("plain text body");
    await mount(txtEntry);
    expect(notesRead).toHaveBeenCalledWith("plan.txt");
    expect(container.querySelector("pre.preview-body")?.textContent).toBe("plain text body");
    expect(container.querySelector("textarea")).toBeNull();
    expect(button("Редактировать")).toBeUndefined();
  });

  it("renders images inline from the binary data URL", async () => {
    notesReadBinaryDataUrl.mockResolvedValue("data:image/png;base64,AAEC");
    await mount(pngEntry);
    expect(notesReadBinaryDataUrl).toHaveBeenCalledWith("pic.png", 20 * 1024 * 1024);
    const img = container.querySelector<HTMLImageElement>(".preview-image img");
    expect(img?.getAttribute("src")).toBe("data:image/png;base64,AAEC");
    expect(notesRead).not.toHaveBeenCalled();
  });

  it("renders PDFs in an iframe from a blob URL", async () => {
    notesReadBinaryDataUrl.mockResolvedValue("data:application/pdf;base64,JVBERw==");
    await mount(pdfEntry);
    expect(notesReadBinaryDataUrl).toHaveBeenCalledWith("doc.pdf", 25 * 1024 * 1024);
    expect(URL.createObjectURL).toHaveBeenCalled();
    const frame = container.querySelector<HTMLIFrameElement>(".preview-pdf iframe");
    expect(frame?.getAttribute("src")).toBe("blob:pdf-preview");
    expect(frame?.getAttribute("title")).toBe("doc.pdf");
  });

  it("shows a name/size/type card for other file types", async () => {
    await mount(zipEntry);
    expect(notesRead).not.toHaveBeenCalled();
    expect(notesReadBinaryDataUrl).not.toHaveBeenCalled();
    expect(container.querySelector(".notes-file-card .file-card-name")?.textContent).toBe(
      "archive.zip",
    );
    expect(container.querySelector(".notes-file-card .file-card-meta")?.textContent).toBe(
      "2.0 КБ · ZIP",
    );
    expect(container.querySelector(".notes-file-card svg")).toBeTruthy();
  });

  it("opens non-md files in the system through open_with with the notes scope", async () => {
    await mount(zipEntry);
    const openBtn = button("Открыть в системе");
    expect(openBtn).toBeTruthy();
    await act(async () => {
      openBtn!.click();
    });
    expect(openWith).toHaveBeenCalledWith("notes", "archive.zip");
  });
});

describe("NoteEditor external reload", () => {
  const fireNotes = async (rel: string) => {
    await act(async () => {
      bus.notes.forEach((h) => h({ relPath: rel }));
    });
    await settle();
  };

  const cmView = (): EditorView => {
    const el = container.querySelector(".cm-editor");
    const view = el ? EditorView.findFromDOM(el as HTMLElement) : null;
    if (!view) throw new Error("CodeMirror view not found");
    return view;
  };

  const setCmValue = (value: string) => {
    const view = cmView();
    act(() => {
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: value } });
    });
  };

  it("silently reloads a clean text file and keeps the scroll position", async () => {
    notesRead.mockResolvedValue("строка 1\nстрока 2");
    await mount(txtEntry);
    const pre = container.querySelector("pre.preview-body")!;
    pre.scrollTop = 42;
    notesRead.mockResolvedValue("новый текст с диска");
    await fireNotes("plan.txt");
    expect(container.querySelector("pre.preview-body")?.textContent).toBe("новый текст с диска");
    expect(container.querySelector("pre.preview-body")?.scrollTop).toBe(42);
    expect(container.querySelector(".preview-conflict")).toBeNull();
  });

  it("shows the «изменён извне» banner instead of overwriting a dirty draft", async () => {
    notesRead.mockResolvedValue("исходник");
    await mount(mdEntry);
    await act(async () => {
      button("Исходник")!.click();
    });
    setCmValue("моя правка");
    notesRead.mockResolvedValue("правка извне");
    await fireNotes("note.md");
    expect(container.querySelector(".preview-conflict")?.textContent).toContain("Файл изменён извне");
    expect(cmView().state.doc.toString()).toBe("моя правка");
    expect(notesWrite).not.toHaveBeenCalled();
  });

  it("«Обновить» replaces the draft with the disk content", async () => {
    notesRead.mockResolvedValue("исходник");
    await mount(mdEntry);
    await act(async () => {
      button("Исходник")!.click();
    });
    setCmValue("моя правка");
    notesRead.mockResolvedValue("правка извне");
    await fireNotes("note.md");
    await act(async () => {
      button("Обновить")!.click();
    });
    await settle();
    expect(cmView().state.doc.toString()).toBe("правка извне");
    expect(container.querySelector(".preview-conflict")).toBeNull();
  });

  it("«Оставить моё» keeps the local draft and dismisses the banner", async () => {
    notesRead.mockResolvedValue("исходник");
    await mount(mdEntry);
    await act(async () => {
      button("Исходник")!.click();
    });
    setCmValue("моя правка");
    notesRead.mockResolvedValue("правка извне");
    await fireNotes("note.md");
    await act(async () => {
      button("Оставить моё")!.click();
    });
    expect(cmView().state.doc.toString()).toBe("моя правка");
    expect(container.querySelector(".preview-conflict")).toBeNull();
  });

  it("reports a deleted/renamed note instead of a broken view", async () => {
    notesRead.mockResolvedValue("было");
    await mount(mdEntry);
    notesRead.mockRejectedValue(new Error("not found"));
    await fireNotes("note.md");
    expect(container.textContent).toContain("Файл недоступен");
    expect(container.querySelector(".md-block")).toBeNull();
  });
});

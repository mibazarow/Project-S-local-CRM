import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { FsEntry } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const fsReadText = vi.hoisted(() => vi.fn(async (_projectId: number, _rel: string) => ""));
const fsWriteText = vi.hoisted(
  () => vi.fn(async (_projectId: number, _rel: string, _content: string) => ({})),
);
const fsReadBinaryDataUrl = vi.hoisted(
  () => vi.fn(async (_projectId: number, _rel: string, _maxBytes: number) => ""),
);
const projectPath = vi.hoisted(() => vi.fn(async (_projectId: number) => "/p/alpha"));

const bus = vi.hoisted(() => ({
  fs: [] as ((p: { projectId: number; relPath: string }) => void)[],
  index: [] as ((p: { projectId: number }) => void)[],
}));
const onFsChanged = vi.hoisted(
  () =>
    vi.fn(async (h: (p: { projectId: number; relPath: string }) => void) => {
      bus.fs.push(h);
      return () => {
        bus.fs = bus.fs.filter((x) => x !== h);
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
  fsReadText,
  fsWriteText,
  fsReadBinaryDataUrl,
  projectPath,
  onFsChanged,
  onIndexChanged,
}));

vi.mock("./useOpenWith", () => ({
  useOpenWith: () => ({
    error: null,
    dialog: null,
    openDocument: vi.fn(),
    showChooser: vi.fn(),
  }),
}));
vi.mock("../shared/crossActions", () => ({
  useFileCrossActions: () => ({ askAgent: vi.fn() }),
}));
vi.mock("../shared/VersionHistory", () => ({ default: () => null }));
vi.mock("./fileOps", () => ({ toMessage: (e: unknown) => String(e) }));

import FilePreview from "./FilePreview";

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
const txtEntry = entry({ name: "a.txt", relPath: "a.txt", ext: "txt", isText: true });

let container: HTMLDivElement;
let root: Root;

const settle = async () => {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
};

const mount = async (e: FsEntry) => {
  await act(async () => {
    root.render(
      <FilePreview projectId={1} entry={e} onClose={() => {}} onDirtyChange={() => {}} />,
    );
  });
  await settle();
};

const button = (label: string) =>
  Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
    (b) => b.textContent === label,
  );

const setValue = (ta: HTMLTextAreaElement, value: string) => {
  const setter = Object.getOwnPropertyDescriptor(
    window.HTMLTextAreaElement.prototype,
    "value",
  )!.set!;
  setter.call(ta, value);
  ta.dispatchEvent(new Event("input", { bubbles: true }));
};

const fireFs = async (rel: string) => {
  await act(async () => {
    bus.fs.forEach((h) => h({ projectId: 1, relPath: rel }));
  });
  await settle();
};

beforeEach(() => {
  fsReadText.mockReset().mockResolvedValue("");
  fsWriteText.mockReset().mockResolvedValue({});
  fsReadBinaryDataUrl.mockReset().mockResolvedValue("");
  projectPath.mockReset().mockResolvedValue("/p/alpha");
  bus.fs = [];
  bus.index = [];
  onFsChanged.mockReset().mockImplementation(async (h: (p: { projectId: number; relPath: string }) => void) => {
    bus.fs.push(h);
    return () => {
      bus.fs = bus.fs.filter((x) => x !== h);
    };
  });
  onIndexChanged.mockReset().mockImplementation(async (h: (p: { projectId: number }) => void) => {
    bus.index.push(h);
    return () => {
      bus.index = bus.index.filter((x) => x !== h);
    };
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("FilePreview external reload", () => {
  it("silently reloads a clean text file and keeps the scroll position", async () => {
    fsReadText.mockResolvedValue("строка 1\nстрока 2");
    await mount(txtEntry);
    const pre = container.querySelector("pre.preview-body")!;
    pre.scrollTop = 30;
    fsReadText.mockResolvedValue("новый текст с диска");
    await fireFs("a.txt");
    expect(container.querySelector("pre.preview-body")?.textContent).toBe("новый текст с диска");
    expect(container.querySelector("pre.preview-body")?.scrollTop).toBe(30);
    expect(container.querySelector(".preview-conflict")).toBeNull();
  });

  it("shows the «изменён извне» banner instead of overwriting a dirty draft", async () => {
    fsReadText.mockResolvedValue("исходник");
    await mount(mdEntry);
    await act(async () => {
      button("Редактировать")!.click();
    });
    const ta = container.querySelector<HTMLTextAreaElement>("textarea.preview-textarea")!;
    setValue(ta, "моя правка");
    fsReadText.mockResolvedValue("правка извне");
    await fireFs("note.md");
    expect(container.querySelector(".preview-conflict")?.textContent).toContain("Файл изменён извне");
    expect(container.querySelector<HTMLTextAreaElement>("textarea.preview-textarea")?.value).toBe(
      "моя правка",
    );
    expect(fsWriteText).not.toHaveBeenCalled();
  });

  it("«Обновить» replaces the draft with the disk content", async () => {
    fsReadText.mockResolvedValue("исходник");
    await mount(mdEntry);
    await act(async () => {
      button("Редактировать")!.click();
    });
    const ta = container.querySelector<HTMLTextAreaElement>("textarea.preview-textarea")!;
    setValue(ta, "моя правка");
    fsReadText.mockResolvedValue("правка извне");
    await fireFs("note.md");
    await act(async () => {
      button("Обновить")!.click();
    });
    await settle();
    expect(container.querySelector<HTMLTextAreaElement>("textarea.preview-textarea")?.value).toBe(
      "правка извне",
    );
    expect(container.querySelector(".preview-conflict")).toBeNull();
  });

  it("«Оставить моё» keeps the local draft and dismisses the banner", async () => {
    fsReadText.mockResolvedValue("исходник");
    await mount(mdEntry);
    await act(async () => {
      button("Редактировать")!.click();
    });
    const ta = container.querySelector<HTMLTextAreaElement>("textarea.preview-textarea")!;
    setValue(ta, "моя правка");
    fsReadText.mockResolvedValue("правка извне");
    await fireFs("note.md");
    await act(async () => {
      button("Оставить моё")!.click();
    });
    expect(container.querySelector<HTMLTextAreaElement>("textarea.preview-textarea")?.value).toBe(
      "моя правка",
    );
    expect(container.querySelector(".preview-conflict")).toBeNull();
  });

  it("reports a deleted/renamed file instead of a broken view", async () => {
    fsReadText.mockResolvedValue("было");
    await mount(mdEntry);
    fsReadText.mockRejectedValue(new Error("not found"));
    await fireFs("note.md");
    expect(container.textContent).toContain("Файл недоступен");
    expect(container.querySelector(".preview-body.md")).toBeNull();
  });
});

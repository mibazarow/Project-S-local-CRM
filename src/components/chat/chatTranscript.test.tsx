import { act, useRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import { ChatTranscript, type ChatItem, type Slice } from "./chatShared";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const items: ChatItem[] = [
  {
    key: 1,
    role: "user",
    text: "Закрепи пример и ответь",
    reasoning: "",
    attachments: [{ name: "смета.pdf", savedPath: "p/smeta.pdf", kind: "document", isImage: false }],
    messageIds: [11],
    pinned: true,
    steps: [],
    compactions: [],
    note: null,
    error: null,
  },
  {
    key: 2,
    role: "agent",
    text: "**Ответ** агента с `кодом`\n\n```js\nconsole.log(1)\n```",
    reasoning: "Сначала посмотрю файл.",
    attachments: [
      { name: "akt.pdf", savedPath: "p/akt.pdf", kind: "document", isImage: false, messageId: 12, index: 0 },
    ],
    messageIds: [12],
    pinned: false,
    steps: [{ key: 21, name: "read_file", summary: "path=a.txt", status: "ok" }],
    compactions: [{ key: 22, summary: "Резюме контекста." }],
    note: "Работа остановлена.",
    error: null,
  },
  {
    key: 3,
    role: "agent",
    text: "",
    reasoning: "",
    attachments: [],
    messageIds: [],
    pinned: false,
    steps: [],
    compactions: [],
    note: null,
    error: "boom",
  },
];

const slice: Slice = {
  loaded: true,
  items,
  run: {
    text: "Стрим ответа…",
    reasoning: "",
    steps: [{ key: 31, name: "write_file", summary: "path=b.txt", status: "run" }],
    compactions: [],
    error: null,
    local: true,
  },
  confirmations: [],
  sendError: null,
  queue: [],
};

function Transcript({ search }: { search: string }) {
  const listRef = useRef<HTMLDivElement>(null);
  return (
    <ChatTranscript
      slice={slice}
      listRef={listRef}
      empty="Пусто"
      projectId={1}
      chatId={10}
      actions={{
        onPin: () => {},
        onRegenerate: () => {},
        onToNote: () => {},
        onToTask: () => {},
      }}
      search={search}
    />
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

describe("ChatTranscript DOM", () => {
  it("renders the representative conversation with a stable markup", async () => {
    await act(async () => {
      root.render(<Transcript search="" />);
    });
    expect(container.innerHTML).toMatchSnapshot();
  });

  it("renders the search-filtered transcript with <mark> highlights", async () => {
    await act(async () => {
      root.render(<Transcript search="пример" />);
    });
    expect(container.innerHTML).toMatchSnapshot();
  });
});

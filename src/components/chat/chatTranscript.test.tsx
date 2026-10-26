import { act, useRef } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import { ChatTranscript } from "./message/ChatTranscript";
import type { ChatItem, Slice } from "./chatTypes";

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
  stopping: false,
  ready: false,
};

function Transcript() {
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
      root.render(<Transcript />);
    });
    expect(container.innerHTML).toMatchSnapshot();
  });
});

describe("provider-silence retry (SPEC 6.0.1, Q23)", () => {
  function errorSlice(retryable: boolean): Slice {
    return {
      loaded: true,
      items: [
        {
          key: 1,
          role: "user",
          text: "сделай отчёт",
          reasoning: "",
          attachments: [],
          messageIds: [11],
          pinned: false,
          steps: [],
          compactions: [],
          note: null,
          error: null,
        },
        {
          key: 2,
          role: "agent",
          text: "",
          reasoning: "",
          attachments: [],
          messageIds: [],
          pinned: false,
          steps: [],
          compactions: [],
          note: null,
          error: "Провайдер не отвечает 180 секунд — проверьте баланс, API-ключ и статус провайдера.",
          retryable,
        },
      ],
      run: null,
      confirmations: [],
      sendError: null,
      queue: [],
      stopping: false,
      ready: false,
    };
  }

  function render(slice: Slice, onRetry: () => void) {
    const listRef = { current: null } as React.RefObject<HTMLDivElement | null>;
    return (
      <ChatTranscript
        slice={slice}
        listRef={listRef}
        empty="Пусто"
        projectId={1}
        chatId={10}
        actions={{ onRetry }}
      />
    );
  }

  it("shows the warning and a single «Повторить» that retries once for a timeout", async () => {
    const onRetry = vi.fn();
    await act(async () => {
      root.render(render(errorSlice(true), onRetry));
    });
    const warning = container.querySelector(".error.timeout");
    expect(warning?.textContent).toContain("Провайдер не отвечает 180 секунд");
    const buttons = [...container.querySelectorAll<HTMLButtonElement>(".error.timeout button")];
    expect(buttons).toHaveLength(1);
    expect(buttons[0].textContent).toBe("Повторить");
    await act(async () => {
      buttons[0].click();
    });
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it("shows no «Повторить» for a non-timeout error", async () => {
    const onRetry = vi.fn();
    await act(async () => {
      root.render(render(errorSlice(false), onRetry));
    });
    expect(container.querySelector(".error.timeout")).toBeNull();
    expect(container.querySelector(".error button")).toBeNull();
    expect(container.querySelector(".error")?.textContent).toContain("Похоже, проблема на стороне API-провайдера");
  });

  it("shows the provider hint when a live run stays silent for a minute", async () => {
    vi.useFakeTimers();
    try {
      const silent: Slice = {
        loaded: true,
        items: [],
        run: { text: "", reasoning: "", steps: [], compactions: [], error: null, local: true },
        confirmations: [],
        sendError: null,
        queue: [],
        stopping: false,
        ready: false,
      };
      const listRef = { current: null } as React.RefObject<HTMLDivElement | null>;
      await act(async () => {
        root.render(
          <ChatTranscript slice={silent} listRef={listRef} empty="Пусто" projectId={1} chatId={10} />,
        );
      });
      expect(container.querySelector(".chat-slow")).toBeNull();
      await act(async () => {
        vi.advanceTimersByTime(60_000);
      });
      expect(container.querySelector(".chat-slow")?.textContent).toContain(
        "Похоже, проблема на стороне API-провайдера",
      );
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("queued messages (Q12)", () => {
  function queuedSlice(): Slice {
    return {
      loaded: true,
      items: [
        {
          key: 1,
          role: "user",
          text: "первое",
          reasoning: "",
          attachments: [],
          messageIds: [11],
          pinned: false,
          steps: [],
          compactions: [],
          note: null,
          error: null,
        },
      ],
      run: { text: "агент отвечает…", reasoning: "", steps: [], compactions: [], error: null, local: true },
      confirmations: [],
      sendError: null,
      queue: ["второе", "третье"],
      stopping: false,
      ready: false,
    };
  }

  function renderQueued(onRemoveQueued: (index: number) => void) {
    const listRef = { current: null } as React.RefObject<HTMLDivElement | null>;
    return (
      <ChatTranscript
        slice={queuedSlice()}
        listRef={listRef}
        empty="Пусто"
        projectId={1}
        chatId={10}
        onRemoveQueued={onRemoveQueued}
      />
    );
  }

  it("renders each queued message immediately, labeled and styled, with a remove control", async () => {
    const onRemoveQueued = vi.fn();
    await act(async () => {
      root.render(renderQueued(onRemoveQueued));
    });
    const queued = [...container.querySelectorAll<HTMLElement>(".msg.queued")];
    expect(queued).toHaveLength(2);
    // Distinct from a normal sent bubble: muted gray bubble, not the accent one.
    expect(queued[0].querySelector(".bubble.queued-bubble")?.textContent).toBe("второе");
    expect(queued[0].querySelector(".bubble.queued-bubble")?.className).toContain("queued-bubble");
    // Small caption above the bubble.
    const label = queued[0].querySelector(".queued-label")?.textContent ?? "";
    expect(label).toContain("В очереди");
    expect(label).toContain("отправится после ответа");
    // Remove control carries the queue position.
    const x = queued[1].querySelector<HTMLButtonElement>(".queued-x");
    expect(x).not.toBeNull();
    await act(async () => {
      x?.click();
    });
    expect(onRemoveQueued).toHaveBeenCalledTimes(1);
    expect(onRemoveQueued).toHaveBeenCalledWith(1);
  });

  it("shows queued messages even when there are no settled items and no run", async () => {
    const listRef = { current: null } as React.RefObject<HTMLDivElement | null>;
    const s: Slice = {
      loaded: true,
      items: [],
      run: null,
      confirmations: [],
      sendError: null,
      queue: ["ждёт"],
      stopping: false,
      ready: false,
    };
    await act(async () => {
      root.render(
        <ChatTranscript slice={s} listRef={listRef} empty="Пусто" projectId={1} chatId={10} />,
      );
    });
    expect(container.querySelector(".chat-empty")).toBeNull();
    expect(container.querySelector(".msg.queued .queued-bubble")?.textContent).toBe("ждёт");
    // No remove control when the view does not provide a handler.
    expect(container.querySelector(".queued-x")).toBeNull();
  });

  it("shows «Отправить оставшееся» for a held queue with no run and fires it once", async () => {
    const listRef = { current: null } as React.RefObject<HTMLDivElement | null>;
    const onFlushQueue = vi.fn();
    const s: Slice = {
      loaded: true,
      items: [],
      run: null,
      confirmations: [],
      sendError: null,
      queue: ["второе", "третье"],
      stopping: false,
      ready: false,
    };
    await act(async () => {
      root.render(
        <ChatTranscript
          slice={s}
          listRef={listRef}
          empty="Пусто"
          projectId={1}
          chatId={10}
          onFlushQueue={onFlushQueue}
        />,
      );
    });
    const btn = [...container.querySelectorAll<HTMLButtonElement>(".queue-flush button")];
    expect(btn).toHaveLength(1);
    expect(btn[0].textContent).toBe("Отправить оставшееся");
    await act(async () => {
      btn[0].click();
    });
    expect(onFlushQueue).toHaveBeenCalledTimes(1);
  });

  it("hides «Отправить оставшееся» while a run is live or the queue is empty", async () => {
    const listRef = { current: null } as React.RefObject<HTMLDivElement | null>;
    const base: Slice = {
      loaded: true,
      items: [],
      run: null,
      confirmations: [],
      sendError: null,
      queue: ["второе"],
      stopping: false,
      ready: false,
    };
    await act(async () => {
      root.render(
        <ChatTranscript
          slice={{ ...base, run: { text: "идёт ответ", reasoning: "", steps: [], compactions: [], error: null, local: true } }}
          listRef={listRef}
          empty="Пусто"
          projectId={1}
          chatId={10}
          onFlushQueue={() => {}}
        />,
      );
    });
    expect(container.querySelector(".queue-flush")).toBeNull();
    await act(async () => {
      root.render(
        <ChatTranscript
          slice={{ ...base, queue: [] }}
          listRef={listRef}
          empty="Пусто"
          projectId={1}
          chatId={10}
          onFlushQueue={() => {}}
        />,
      );
    });
    expect(container.querySelector(".queue-flush")).toBeNull();
  });
});

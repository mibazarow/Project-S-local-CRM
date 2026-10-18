import { useEffect, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import type { AgentEventPayload } from "../../api/types";
import {
  historyToItems,
  toMessage,
  ChatTranscript,
  ChatHeaderTools,
  ConfirmCard,
  countMatches,
  downloadMarkdown,
  resolvePinMessageId,
  resolveRegenerateTurn,
  runChatExport,
  type ChatItem,
} from "../chat/chatShared";
import { useChatRun } from "../chat/useChatRun";
import { ChatComposer } from "../chat/ChatComposer";

// One continuous notes chat per working dir (SPEC notes chat slice): the
// backend get-or-creates the singleton chat and routes its runs as
// "agent-event" / "agent-confirmation" with notes: true, keyed by chatId.
export default function NotesChat() {
  const { actions } = useApp();
  const [chatId, setChatId] = useState<number | null>(null);
  const chatIdRef = useRef<number | null>(null);
  const [text, setText] = useState("");
  const [chatQuery, setChatQuery] = useState("");
  const listRef = useRef<HTMLDivElement>(null);

  const reloadHistory = (key: number, dispatch: ReturnType<typeof useChatRun>["dispatch"]) => {
    void ipc
      .notesChatHistory()
      .then((dtos) => dispatch({ type: "loaded", key, items: historyToItems(dtos) }))
      .catch(() => {});
  };

  // The notes chat is a per-working-dir singleton: agent events route by the
  // notes:true flag (keyed by the event's chatId), while confirmations carry no
  // chat id so they key by the locally known id; agent-file/compaction events
  // match that same id. A run that finished while this view was unmounted (a
  // remote run) finalizes from the persisted transcript.
  const { state, dispatch } = useChatRun(
    {
      routeAgentEvent: (p: AgentEventPayload) => (p.notes === true ? p.chatId : null),
      routeConfirmation: (p) => (p.notes === true ? chatIdRef.current : null),
      routeAgentFile: (p) => (p.chatId === chatIdRef.current ? p.chatId : null),
      routeCompaction: (p) => (p.chatId === chatIdRef.current ? p.chatId : null),
      onRunEnd: (key, run, dispatch) => {
        if (!run || !run.local) reloadHistory(key, dispatch);
        actions.notifyTasksChanged();
      },
      sendQueued: (key, combined, dispatch) => {
        void ipc
          .sendNotesMessage(combined)
          .catch((e) => dispatch({ type: "sendFailed", key, message: toMessage(e) }));
      },
    },
    [actions],
  );

  const slice = chatId != null ? (state[chatId] ?? null) : null;
  const running = slice?.run != null;
  const confirmation = slice?.confirmations[0] ?? null;

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const id = await ipc.ensureNotesChat();
        if (cancelled) return;
        chatIdRef.current = id;
        setChatId(id);
        const dtos = await ipc.notesChatHistory();
        if (!cancelled) dispatch({ type: "loaded", key: id, items: historyToItems(dtos) });
      } catch {
        // The chat stays unavailable until the next mount; the composer
        // stays disabled and no history is shown.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [state]);

  const sendText = (value: string) => {
    if (chatId == null || running) return;
    const trimmed = value.trim();
    if (!trimmed) return;
    setText("");
    dispatch({ type: "send", key: chatId, text: trimmed, attachments: [] });
    void ipc
      .sendNotesMessage(trimmed)
      .catch((e) => dispatch({ type: "sendFailed", key: chatId, message: toMessage(e) }));
  };

  // While a run is live the composer stays enabled: sending queues the text
  // (flushed as one combined turn when the run finishes).
  const send = () => {
    if (chatId == null) return;
    const trimmed = text.trim();
    if (!trimmed) return;
    if (running) {
      dispatch({ type: "enqueue", key: chatId, text: trimmed });
      setText("");
      return;
    }
    sendText(text);
  };

  const resolve = (decision: "allow" | "deny" | "allowAlways") => {
    if (!confirmation || chatId == null) return;
    void ipc.resolveConfirmation(confirmation.id, decision).catch(() => {});
    dispatch({ type: "confirmResolved", key: chatId, id: confirmation.id });
  };

  const stop = () => {
    void ipc.cancelNotesRun().catch(() => {});
  };

  // Message-level UX: live items carry no DB ids, so both handlers refetch
  // the history and map the item by position before touching the transcript.
  const pinItem = async (item: ChatItem, pinned: boolean) => {
    if (chatId == null) return;
    const messageId = await resolvePinMessageId(
      () => ipc.notesChatHistory(),
      slice?.items ?? [],
      item.key,
    );
    if (!messageId) return;
    try {
      await ipc.chatPin(chatId, messageId, pinned);
      dispatch({ type: "pin", key: chatId, itemKey: item.key, pinned });
    } catch {
      // The pin flag simply stays as it was.
    }
  };

  const regenerateItem = async (item: ChatItem) => {
    if (chatId == null || running) return;
    const turn = await resolveRegenerateTurn(
      () => ipc.notesChatHistory(),
      slice?.items ?? [],
      item.key,
    );
    if (!turn) return;
    try {
      await ipc.chatTruncateAfter(chatId, turn.messageId);
      dispatch({ type: "regenerate", key: chatId, itemKey: item.key });
      await ipc
        .sendNotesMessage(turn.userText)
        .catch((e) => dispatch({ type: "sendFailed", key: chatId, message: toMessage(e) }));
    } catch (e) {
      dispatch({ type: "sendFailed", key: chatId, message: toMessage(e) });
    }
  };

  const exportChat = () => {
    if (chatId == null) return;
    runChatExport(chatId, (r) => downloadMarkdown(r.markdown, `notes-chat-${chatId}.md`));
  };

  return (
    <aside className="chat">
      <div className="chat-head">
        <span className="chat-title">Чат с заметками</span>
        {running && <span className="chat-sub">агент работает…</span>}
        <ChatHeaderTools
          query={chatQuery}
          onQuery={setChatQuery}
          matchCount={countMatches(slice?.items ?? [], chatQuery)}
          onExport={exportChat}
          exportDisabled={chatId == null}
        />
      </div>

      <ChatTranscript
        slice={slice}
        listRef={listRef}
        empty="Спросите агента о заметках…"
        projectId={null}
        chatId={chatId}
        actions={{ onPin: pinItem, onRegenerate: regenerateItem }}
        search={chatQuery}
      />

      {slice?.sendError && (
        <div className="chat-error">
          {slice.sendError}
          <br />
          Проверьте Настройки → Модель.
        </div>
      )}

      {confirmation && <ConfirmCard confirmation={confirmation} onResolve={resolve} />}

      <ChatComposer
        text={text}
        onTextChange={setText}
        onSend={send}
        onStop={stop}
        onClear={() => setText("")}
        running={running}
        queueCount={slice?.queue.length ?? 0}
        placeholder="Спросите агента о заметках…"
        disabled={chatId == null}
        sendDisabled={chatId == null}
      />
    </aside>
  );
}

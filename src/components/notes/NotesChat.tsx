import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import { useAppActions } from "../../state/AppStore";
import {
  NOTES_KEY,
  dispatch,
  ensureSubscribed,
  getSlice,
  openChat,
  closeChat,
  onRunEnd,
  rememberChatId,
  send as storeSend,
  useChatSlice,
} from "../../state/chatRunStore";
import {
  historyToItems,
  toMessage,
  ChatTranscript,
  ConfirmCard,
  resolvePinMessageId,
  resolveRegenerateTurn,
  type ChatItem,
} from "../chat/chatShared";
import { ChatComposer } from "../chat/ChatComposer";

// One continuous notes chat per working dir (SPEC notes chat slice): the
// backend get-or-creates the singleton chat and routes its runs as
// "agent-event" / "agent-confirmation" with notes: true. The run state lives
// in the global store under the fixed NOTES_KEY, so it survives this view
// unmounting and keeps streaming while another tab is open.
export default function NotesChat() {
  const actions = useAppActions();
  const [chatId, setChatId] = useState<number | null>(null);
  const chatIdRef = useRef<number | null>(null);
  const [text, setText] = useState("");
  const listRef = useRef<HTMLDivElement>(null);

  const slice = useChatSlice(NOTES_KEY);
  const running = slice?.run != null;
  const confirmation = slice?.confirmations[0] ?? null;

  const reloadHistory = () => {
    void ipc
      .notesChatHistory()
      .then((dtos) => dispatch({ type: "loaded", key: NOTES_KEY, items: historyToItems(dtos) }))
      .catch(() => {});
  };

  useEffect(() => {
    ensureSubscribed();
    openChat(NOTES_KEY);
    return () => closeChat(NOTES_KEY);
  }, []);

  // A run that finished while this view was unmounted (a remote run) finalizes
  // from the persisted transcript; refresh the kanban defensively either way.
  useEffect(
    () =>
      onRunEnd((_key, wasLocal) => {
        if (!wasLocal) reloadHistory();
        actions.notifyTasksChanged();
      }),
    [actions],
  );

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const id = await ipc.ensureNotesChat();
        if (cancelled) return;
        chatIdRef.current = id;
        setChatId(id);
        rememberChatId(NOTES_KEY, id);
        const dtos = await ipc.notesChatHistory();
        if (!cancelled) dispatch({ type: "loaded", key: NOTES_KEY, items: historyToItems(dtos) });
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
  }, [slice]);

  const sendText = (value: string) => {
    if (chatId == null || running) return;
    const trimmed = value.trim();
    if (!trimmed) return;
    setText("");
    storeSend(NOTES_KEY, trimmed, []);
  };

  // While a run is live the composer stays enabled: sending queues the text
  // (flushed as one combined turn when the run finishes).
  const send = () => {
    if (chatId == null) return;
    const trimmed = text.trim();
    if (!trimmed) return;
    if (running) {
      dispatch({ type: "enqueue", key: NOTES_KEY, text: trimmed });
      setText("");
      return;
    }
    sendText(text);
  };

  const resolve = (decision: "allow" | "deny" | "allowAlways") => {
    if (!confirmation) return;
    void ipc.resolveConfirmation(confirmation.id, decision).catch(() => {});
    dispatch({ type: "confirmResolved", key: NOTES_KEY, id: confirmation.id });
  };

  const stop = () => {
    void ipc.cancelNotesRun().catch(() => {});
  };

  // Message-level UX: live items carry no DB ids, so both handlers refetch
  // the history and map the item by position before touching the transcript.
  const pinItem = useCallback(
    async (item: ChatItem, pinned: boolean) => {
      const id = chatIdRef.current;
      if (id == null) return;
      const messageId = await resolvePinMessageId(
        () => ipc.notesChatHistory(),
        getSlice(NOTES_KEY)?.items ?? [],
        item.key,
      );
      if (!messageId) return;
      try {
        await ipc.chatPin(id, messageId, pinned);
        dispatch({ type: "pin", key: NOTES_KEY, itemKey: item.key, pinned });
      } catch {
        // The pin flag simply stays as it was.
      }
    },
    [],
  );

  const regenerateItem = useCallback(
    async (item: ChatItem) => {
      const id = chatIdRef.current;
      if (id == null) return;
      const sliceNow = getSlice(NOTES_KEY);
      if (sliceNow?.run != null) return;
      const turn = await resolveRegenerateTurn(
        () => ipc.notesChatHistory(),
        sliceNow?.items ?? [],
        item.key,
      );
      if (!turn) return;
      try {
        await ipc.chatTruncateAfter(id, turn.messageId);
        dispatch({ type: "regenerate", key: NOTES_KEY, itemKey: item.key });
        void ipc
          .sendNotesMessage(turn.userText)
          .catch((e) => dispatch({ type: "sendFailed", key: NOTES_KEY, message: toMessage(e) }));
      } catch (e) {
        dispatch({ type: "sendFailed", key: NOTES_KEY, message: toMessage(e) });
      }
    },
    [],
  );

  const transcriptActions = useMemo(
    () => ({ onPin: pinItem, onRegenerate: regenerateItem, onRetry: regenerateItem }),
    [pinItem, regenerateItem],
  );

  return (
    <aside className="chat">
      <div className="chat-head">
        <span className="chat-title">Чат с заметками</span>
        {running && <span className="chat-sub">агент работает…</span>}
      </div>

      <ChatTranscript
        slice={slice}
        listRef={listRef}
        empty="Спросите агента о заметках…"
        projectId={null}
        chatId={chatId}
        actions={transcriptActions}
        onRemoveQueued={(index) => dispatch({ type: "removeFromQueue", key: NOTES_KEY, index })}
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

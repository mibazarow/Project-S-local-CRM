import { useEffect, useReducer, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import type { AgentEventPayload } from "../../api/types";
import {
  chatReducer,
  historyToItems,
  summarizeArgs,
  toMessage,
  ChatTranscript,
  ConfirmCard,
} from "../chat/chatShared";

// One continuous notes chat per working dir (SPEC notes chat slice): the
// backend get-or-creates the singleton chat and routes its runs as
// "agent-event" / "agent-confirmation" with notes: true, keyed by chatId.
export default function NotesChat() {
  const { actions } = useApp();
  const [state, dispatch] = useReducer(chatReducer, {});
  const stateRef = useRef(state);
  stateRef.current = state;
  const [chatId, setChatId] = useState<number | null>(null);
  const chatIdRef = useRef<number | null>(null);
  const [text, setText] = useState("");
  const taRef = useRef<HTMLTextAreaElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

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
    let cancelled = false;
    let unEvent: (() => void) | undefined;
    let unConfirm: (() => void) | undefined;
    let unFile: (() => void) | undefined;
    const reloadHistory = (key: number) => {
      void ipc
        .notesChatHistory()
        .then((dtos) => dispatch({ type: "loaded", key, items: historyToItems(dtos) }))
        .catch(() => {});
    };
    const onEvent = (p: AgentEventPayload) => {
      if (p.notes !== true) return; // project/harness payloads carry their own keys
      const key = p.chatId;
      switch (p.event.type) {
        case "textDelta":
          dispatch({ type: "delta", key, text: p.event.text });
          break;
        case "reasoningDelta":
          dispatch({ type: "reasoningDelta", key, text: p.event.text });
          break;
        case "toolCallStarted":
          dispatch({ type: "stepStart", key, name: p.event.name, summary: summarizeArgs(p.event.args) });
          break;
        case "toolCallFinished":
          dispatch({ type: "stepFinish", key, ok: p.event.ok });
          break;
        case "done": {
          const run = stateRef.current[key]?.run;
          dispatch({
            type: "done",
            key,
            answer: p.event.answer,
            reason: p.event.reason,
            reasoning: p.event.reasoning,
          });
          // A run started while this view was unmounted finalizes from the
          // persisted transcript, not from the stream.
          if (!run || !run.local) reloadHistory(key);
          // Defensive: refresh the kanban after every run, whatever the
          // agent touched (its change events can race the run's done event).
          actions.notifyTasksChanged();
          break;
        }
        case "error": {
          const run = stateRef.current[key]?.run;
          dispatch({ type: "runError", key, message: p.event.message });
          if (!run || !run.local) reloadHistory(key);
          actions.notifyTasksChanged();
          break;
        }
        case "confirmationRequested":
          break; // the id-bearing copy arrives as "agent-confirmation"
      }
    };
    void ipc.onAgentEvent(onEvent).then((u) => (cancelled ? u() : (unEvent = u)));
    void ipc
      .onAgentConfirmation((p) => {
        if (p.notes !== true) return;
        // The confirmation payload carries no chat id: the notes chat is a
        // per-working-dir singleton, so the locally known id is the key.
        const key = chatIdRef.current;
        if (key == null) return;
        dispatch({ type: "confirm", key, payload: p });
      })
      .then((u) => (cancelled ? u() : (unConfirm = u)));
    // Agent-sent files: the notes chat is a per-working-dir singleton, so the
    // event's chatId matches the locally known id; the reducer dedupes by
    // messageId, so a later history reload never renders the file twice.
    void ipc
      .onAgentFile((p) => {
        if (p.chatId !== chatIdRef.current) return;
        dispatch({ type: "agentFile", key: p.chatId, payload: p });
      })
      .then((u) => (cancelled ? u() : (unFile = u)));
    return () => {
      cancelled = true;
      unEvent?.();
      unConfirm?.();
      unFile?.();
    };
  }, [actions]);

  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [state]);

  const grow = () => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    const cs = getComputedStyle(ta);
    const lh = parseFloat(cs.lineHeight) || parseFloat(cs.fontSize) * 1.5;
    ta.style.height = Math.min(ta.scrollHeight, 10 * lh + 16) + "px";
  };

  const send = () => {
    if (chatId == null || running) return;
    const trimmed = text.trim();
    if (!trimmed) return;
    setText("");
    grow();
    dispatch({ type: "send", key: chatId, text: trimmed, attachments: [] });
    void ipc
      .sendNotesMessage(trimmed)
      .catch((e) => dispatch({ type: "sendFailed", key: chatId, message: toMessage(e) }));
  };

  const resolve = (decision: "allow" | "deny" | "allowAlways") => {
    if (!confirmation || chatId == null) return;
    void ipc.resolveConfirmation(confirmation.id, decision).catch(() => {});
    dispatch({ type: "confirmResolved", key: chatId, id: confirmation.id });
  };

  const stop = () => {
    void ipc.cancelNotesRun().catch(() => {});
  };

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
      />

      {slice?.sendError && (
        <div className="chat-error">
          {slice.sendError}
          <br />
          Проверьте Настройки → Модель.
        </div>
      )}

      {confirmation && <ConfirmCard confirmation={confirmation} onResolve={resolve} />}

      <div className="chat-input">
        <textarea
          ref={taRef}
          rows={3}
          placeholder="Спросите агента о заметках…"
          value={text}
          disabled={chatId == null}
          onChange={(e) => {
            setText(e.target.value);
            grow();
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              send();
            }
          }}
        />
        <div className="chat-tools">
          <span className="chat-hint">Shift+Enter — перенос строки, Enter — отправить</span>
          {running ? (
            <button className="ghost-btn small stop-btn" type="button" onClick={stop}>
              Стоп
            </button>
          ) : (
            <button
              className="icon-btn clear-btn"
              type="button"
              title="Очистить"
              onClick={() => {
                setText("");
                grow();
              }}
            >
              <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"><path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6" /></svg>
            </button>
          )}
          <button
            className="icon-btn send-btn"
            type="button"
            title="Отправить"
            disabled={running || chatId == null || !text.trim()}
            onClick={send}
          >
            <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="m22 2-7 20-4-9-9-4 20-7z" /></svg>
          </button>
        </div>
      </div>
    </aside>
  );
}

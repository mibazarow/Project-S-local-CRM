import { useEffect, useReducer, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import type { AgentEventPayload, MessageAttachmentDto } from "../../api/types";
import {
  chatReducer,
  historyToItems,
  summarizeArgs,
  toMessage,
  ChatTranscript,
  ConfirmCard,
  AttachFileIcon,
  AttachXIcon,
  useVisionSupport,
} from "./chatShared";
import { useChatAttachments } from "./useChatAttachments";
import TodoPanel from "./TodoPanel";

interface Props {
  projectId: number | null;
  projectName: string | null;
}

export default function ProjectChat({ projectId, projectName }: Props) {
  const [state, dispatch] = useReducer(chatReducer, {});
  const stateRef = useRef(state);
  stateRef.current = state;
  const [text, setText] = useState("");
  const taRef = useRef<HTMLTextAreaElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const chatRootRef = useRef<HTMLElement | null>(null);
  const att = useChatAttachments(chatRootRef);
  const visionSupported = useVisionSupport();

  const slice = projectId != null ? (state[projectId] ?? null) : null;
  const running = slice?.run != null;
  const confirmation = slice?.confirmations[0] ?? null;

  useEffect(() => {
    if (projectId == null) return;
    if (stateRef.current[projectId]?.loaded) return;
    void ipc
      .projectChatHistory(projectId)
      .then((dtos) => dispatch({ type: "loaded", key: projectId, items: historyToItems(dtos) }))
      .catch(() => dispatch({ type: "loaded", key: projectId, items: [] }));
  }, [projectId]);

  useEffect(() => {
    let cancelled = false;
    let unEvent: (() => void) | undefined;
    let unConfirm: (() => void) | undefined;
    const onEvent = (p: AgentEventPayload) => {
      if (p.projectId == null) return; // harness-session events carry sessionId
      const key = p.projectId;
      switch (p.event.type) {
        case "textDelta":
          dispatch({ type: "delta", key, text: p.event.text });
          break;
        case "toolCallStarted":
          dispatch({ type: "stepStart", key, name: p.event.name, summary: summarizeArgs(p.event.args) });
          break;
        case "toolCallFinished":
          dispatch({ type: "stepFinish", key, ok: p.event.ok });
          break;
        case "done":
          dispatch({ type: "done", key, answer: p.event.answer, reason: p.event.reason });
          break;
        case "error":
          dispatch({ type: "runError", key, message: p.event.message });
          break;
        case "confirmationRequested":
          break; // the id-bearing copy arrives as "agent-confirmation"
      }
    };
    void ipc.onAgentEvent(onEvent).then((u) => (cancelled ? u() : (unEvent = u)));
    void ipc
      .onAgentConfirmation((p) => {
        if (p.projectId == null) return;
        dispatch({ type: "confirm", key: p.projectId, payload: p });
      })
      .then((u) => (cancelled ? u() : (unConfirm = u)));
    return () => {
      cancelled = true;
      unEvent?.();
      unConfirm?.();
    };
  }, []);

  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [slice]);

  const grow = () => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    const cs = getComputedStyle(ta);
    const lh = parseFloat(cs.lineHeight) || parseFloat(cs.fontSize) * 1.5;
    ta.style.height = Math.min(ta.scrollHeight, 10 * lh + 16) + "px";
  };

  const send = () => {
    if (projectId == null || running) return;
    const trimmed = text.trim();
    if (!trimmed) return;
    void (async () => {
      let refs: MessageAttachmentDto[] = [];
      if (att.pending.length > 0) {
        try {
          const saved = await ipc.attachFiles(
            projectId,
            null,
            att.pending.map((a) => a.path),
          );
          refs = saved.map((a) => ({
            name: a.name,
            savedPath: a.savedPath,
            kind: a.kind,
            isImage: a.isImage,
          }));
        } catch (e) {
          dispatch({ type: "sendFailed", key: projectId, message: toMessage(e) });
          return;
        }
      }
      setText("");
      grow();
      att.clear();
      dispatch({ type: "send", key: projectId, text: trimmed, attachments: refs });
      void ipc.sendProjectMessage(projectId, trimmed, refs).catch((e) =>
        dispatch({ type: "sendFailed", key: projectId, message: toMessage(e) }),
      );
    })();
  };

  const resolve = (decision: "allow" | "deny" | "allowAlways") => {
    if (!confirmation || projectId == null) return;
    void ipc.resolveConfirmation(confirmation.id, decision).catch(() => {});
    dispatch({ type: "confirmResolved", key: projectId, id: confirmation.id });
  };

  const stop = () => {
    if (projectId != null) void ipc.cancelRun(projectId).catch(() => {});
  };

  return (
    <aside
      className={att.dragOver ? "chat drag-over" : "chat"}
      ref={chatRootRef}
      data-chat-drop
    >
      <div className="chat-head">
        <span className="chat-title">Чат проекта «{projectName ?? "—"}»</span>
        {running && <span className="chat-sub">агент работает…</span>}
      </div>

      <TodoPanel scopeKind="project" scopeId={projectId} />

      <ChatTranscript
        slice={slice}
        listRef={listRef}
        empty="Начните диалог — агент проекта ответит здесь."
        projectId={projectId}
      />

      {att.pending.length > 0 && (
        <div className="attach-bar">
          {att.pending.map((a) => (
            <div key={a.key} className="attach-chip">
              {a.isImage && a.previewUrl ? <img src={a.previewUrl} alt="" /> : <AttachFileIcon />}
              <span className="nm">{a.name}</span>
              <button className="x" type="button" title="Убрать" onClick={() => att.remove(a.key)}>
                <AttachXIcon />
              </button>
            </div>
          ))}
        </div>
      )}

      {att.pending.some((a) => a.isImage) && visionSupported !== true && (
        <div className="vision-warn">
          Модель не поддерживает vision — изображение не будет распознано (сохранится в проект)
        </div>
      )}

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
          placeholder="Спроси агента о проекте…"
          value={text}
          disabled={projectId == null}
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
          <button
            className="icon-btn attach-btn"
            type="button"
            title="Приложить файл или картинку"
            disabled={projectId == null}
            onClick={() => void att.pick().catch(() => {})}
          >
            <svg viewBox="0 0 24 24" width="17" height="17" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"><path d="m21.4 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l9.2-9.19a4 4 0 0 1 5.65 5.66l-9.2 9.19a2 2 0 0 1-2.82-2.83l8.49-8.48" /></svg>
          </button>
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
                att.clear();
              }}
            >
              <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"><path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6" /></svg>
            </button>
          )}
          <button
            className="icon-btn send-btn"
            type="button"
            title="Отправить"
            disabled={running || !text.trim()}
            onClick={send}
          >
            <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="m22 2-7 20-4-9-9-4 20-7z" /></svg>
          </button>
        </div>
      </div>
    </aside>
  );
}

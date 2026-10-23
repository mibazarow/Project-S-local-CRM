import { type RefObject } from "react";
import type { ConfirmationRequestPayload, SessionDto } from "../../api/types";
import type { Slice, TranscriptActions } from "../chat/chatTypes";
import { ChatTranscript } from "../chat/message/ChatTranscript";
import { ConfirmCard } from "../chat/actions/ConfirmCard";
import { AttachFileIcon, AttachXIcon } from "../chat/attachments/MessageAttachments";
import { ChatComposer } from "../chat/composer/ChatComposer";
import { useChatAttachments } from "../chat/attachments/useChatAttachments";
import TodoPanel from "../chat/TodoPanel";
import { dispatch, flushHeldQueue, sessionKey } from "../../state/chatRunStore";
import { UNTITLED } from "./shared";

export function HarnessChatPanel({
  att,
  chatRootRef,
  listRef,
  activeSession,
  activeId,
  chatId,
  slice,
  running,
  hasSessions,
  sessionsError,
  branchNote,
  confirmation,
  transcriptActions,
  visionSupported,
  text,
  onTextChange,
  onSend,
  onStop,
  onClear,
  onResolve,
}: {
  att: ReturnType<typeof useChatAttachments>;
  chatRootRef: RefObject<HTMLElement | null>;
  listRef: RefObject<HTMLDivElement | null>;
  activeSession: SessionDto | null;
  activeId: number | null;
  chatId: number | null;
  slice: Slice | null;
  running: boolean;
  hasSessions: boolean;
  sessionsError: string | null;
  branchNote: string | null;
  confirmation: ConfirmationRequestPayload | null;
  transcriptActions: TranscriptActions;
  visionSupported: boolean | null;
  text: string;
  onTextChange: (text: string) => void;
  onSend: () => void;
  onStop: () => void;
  onClear: () => void;
  onResolve: (decision: "allow" | "deny" | "allowAlways") => void;
}) {
  return (
    <section
      className={att.dragOver ? "center center-harness drag-over" : "center center-harness"}
      ref={chatRootRef}
      data-chat-drop
    >
      <div className="center-head">
        <h2>{activeSession ? activeSession.title ?? UNTITLED : "Харнес"}</h2>
        {running && <span className="chat-sub">агент работает…</span>}
      </div>

      {branchNote && <div className="set-hint harness-branch-note">{branchNote}</div>}

      <TodoPanel scopeKind="session" scopeId={activeId} />

      <ChatTranscript
        slice={slice}
        listRef={listRef}
        className="chat-messages harness-messages"
        header={sessionsError ? <div className="chat-error">{sessionsError}</div> : undefined}
        empty={
          !hasSessions && !sessionsError ? (
            <>
              Сессий пока нет.
              <br />
              Напишите сообщение — сессия создастся автоматически, или нажмите «+» слева.
            </>
          ) : (
            "Начните диалог — общий агент ответит здесь."
          )
        }
        projectId={null}
        chatId={chatId}
        actions={transcriptActions}
        onRemoveQueued={
          activeId != null
            ? (index) => dispatch({ type: "removeFromQueue", key: sessionKey(activeId), index })
            : undefined
        }
        onFlushQueue={activeId != null ? () => flushHeldQueue(sessionKey(activeId)) : undefined}
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

      {confirmation && <ConfirmCard confirmation={confirmation} onResolve={onResolve} />}

      <ChatComposer
        text={text}
        onTextChange={onTextChange}
        onSend={onSend}
        onStop={onStop}
        onClear={onClear}
        running={running}
        stopping={slice?.stopping ?? false}
        queueCount={slice?.queue.length ?? 0}
        placeholder="Поручение общему агенту…"
        attach={{ onPick: () => att.pick().catch(() => {}) }}
      />
    </section>
  );
}

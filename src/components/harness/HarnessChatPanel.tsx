import { type RefObject, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import type { ConfirmationRequestPayload, SessionDto } from "../../api/types";
import type { Slice, TranscriptActions } from "../chat/chatTypes";
import { ChatTranscript } from "../chat/message/ChatTranscript";
import { errorHint } from "../chat/errorHint";
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
  onSetWorkingDir,
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
  onSetWorkingDir: (path: string | null) => void;
}) {
  const [picking, setPicking] = useState(false);
  const boundDir = activeSession?.workingDir ?? null;
  const folderName = boundDir ? (boundDir.split("/").filter(Boolean).pop() ?? boundDir) : null;

  // The chip opens a native folder picker; the binding is per chat, so the
  // agent's file tools stay inside the chosen folder (Q.md folder binding).
  const pickFolder = async () => {
    setPicking(true);
    try {
      const picked = await open({ directory: true, title: "Папка для этого чата" });
      if (typeof picked === "string" && picked) onSetWorkingDir(picked);
    } finally {
      setPicking(false);
    }
  };

  return (
    <section
      className={att.dragOver ? "center center-harness drag-over" : "center center-harness"}
      ref={chatRootRef}
      data-chat-drop
    >
      <div className="center-head">
        <h2>{activeSession ? activeSession.title ?? UNTITLED : "Харнес"}</h2>
        {running && <span className="chat-sub">агент работает…</span>}
        {activeSession && (
          <div className="chat-scope">
            <button
              type="button"
              className={boundDir ? "chat-scope-btn bound" : "chat-scope-btn"}
              onClick={() => void pickFolder()}
              disabled={picking}
              title={
                boundDir
                  ? `Агент работает только в этой папке: ${boundDir}`
                  : "Агент работает в общей рабочей папке. Нажмите, чтобы выбрать отдельную папку для этого чата."
              }
            >
              <span className="chat-scope-icon" aria-hidden="true">📁</span>
              <span className="chat-scope-label">{folderName ?? "Общая папка"}</span>
            </button>
            {boundDir && (
              <button
                type="button"
                className="chat-scope-reset"
                onClick={() => onSetWorkingDir(null)}
                title="Вернуть общую рабочую папку"
                aria-label="Вернуть общую рабочую папку"
              >
                ×
              </button>
            )}
          </div>
        )}
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
          {errorHint(slice.sendError)}
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

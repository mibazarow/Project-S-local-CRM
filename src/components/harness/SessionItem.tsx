import { useState } from "react";
import type { SessionDto } from "../../api/types";
import { formatActivity } from "../../shared/format";
import { IconPin } from "../files/icons";
import { TrashIcon, UNTITLED } from "./shared";

const PencilIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z" />
  </svg>
);

export function SessionItem({
  session,
  active,
  running,
  ready,
  onOpen,
  onRename,
  onDelete,
  onTogglePin,
}: {
  session: SessionDto;
  active: boolean;
  running: boolean;
  ready: boolean;
  onOpen: () => void;
  onRename: (id: number, title: string) => void;
  onDelete: () => void;
  onTogglePin: () => void;
}) {
  const [renaming, setRenaming] = useState(false);
  const [draft, setDraft] = useState(session.title ?? "");

  const startRename = () => {
    setDraft(session.title ?? "");
    setRenaming(true);
  };

  const commit = () => {
    setRenaming(false);
    const next = draft.trim();
    if (next && next !== (session.title ?? "")) onRename(session.id, next);
  };

  return (
    <div
      className={active ? "session-item active" : "session-item"}
      role="button"
      tabIndex={0}
      onClick={() => !renaming && onOpen()}
      onKeyDown={(e) => {
        if (e.key === "Enter" && !renaming) onOpen();
      }}
    >
      {renaming ? (
        <input
          className="rename-input"
          value={draft}
          autoFocus
          placeholder={UNTITLED}
          onClick={(e) => e.stopPropagation()}
          onMouseDown={(e) => e.stopPropagation()}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
            if (e.key === "Escape") setRenaming(false);
          }}
        />
      ) : (
        <span className="name">
          {running && <span className="chat-status running" title="Агент работает" />}
          {!running && ready && <span className="chat-status ready" title="Ответ готов" />}
          {session.title ?? UNTITLED}
        </span>
      )}
      <span className="meta">{formatActivity(session.lastActiveAt)}</span>
      <button
        className={session.pinned ? "session-pin pinned" : "session-pin"}
        type="button"
        title={session.pinned ? "Открепить чат" : "Закрепить чат"}
        aria-pressed={session.pinned}
        onClick={(e) => {
          e.stopPropagation();
          onTogglePin();
        }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <IconPin />
      </button>
      <span className="session-actions">
        <button
          className="mini-btn"
          type="button"
          title="Переименовать сессию"
          onClick={(e) => {
            e.stopPropagation();
            startRename();
          }}
        >
          <PencilIcon />
        </button>
        <button
          className="mini-btn danger"
          type="button"
          title="Удалить сессию"
          onClick={(e) => {
            e.stopPropagation();
            onDelete();
          }}
        >
          <TrashIcon />
        </button>
      </span>
    </div>
  );
}

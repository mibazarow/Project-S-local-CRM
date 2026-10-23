import type { SessionDto } from "../../api/types";
import type { ChatState } from "../chat/chatTypes";
import { sessionKey } from "../../state/chatRunStore";
import { SessionItem } from "./SessionItem";

const RefreshIcon = () => (
  <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M21 12a9 9 0 1 1-2.64-6.36M21 3v6h-6" />
  </svg>
);

export function SessionsSidebar({
  sessions,
  activeId,
  chat,
  indexNote,
  onReindex,
  onCreate,
  onOpen,
  onRename,
  onDelete,
  onTogglePin,
}: {
  sessions: SessionDto[];
  activeId: number | null;
  chat: ChatState;
  indexNote: string | null;
  onReindex: () => void;
  onCreate: () => void;
  onOpen: (id: number) => void;
  onRename: (id: number, title: string) => void;
  onDelete: (session: SessionDto) => void;
  onTogglePin: (session: SessionDto) => void;
}) {
  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <span className="sidebar-title">Сессии</span>
        <span className="sidebar-head-actions">
          <button
            className="icon-btn small"
            type="button"
            title="Обновить индекс"
            onClick={onReindex}
          >
            <RefreshIcon />
          </button>
          <button className="new-chat-btn" type="button" title="Новая сессия" onClick={onCreate}>
            <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" aria-hidden="true"><path d="M12 5v14M5 12h14" /></svg>
            <span className="new-chat-label">Новый чат</span>
          </button>
        </span>
      </div>
      {indexNote && <div className="set-hint harness-index-note">{indexNote}</div>}
      <div className="session-list">
        {sessions.map((s) => (
          <SessionItem
            key={s.id}
            session={s}
            active={s.id === activeId}
            running={chat[sessionKey(s.id)]?.run != null}
            ready={chat[sessionKey(s.id)]?.ready === true}
            onOpen={() => onOpen(s.id)}
            onRename={onRename}
            onDelete={() => onDelete(s)}
            onTogglePin={() => onTogglePin(s)}
          />
        ))}
        {sessions.length === 0 && (
          <div className="empty-note">
            Сессий пока нет. Напишите сообщение или нажмите «+», чтобы создать первую.
          </div>
        )}
      </div>
      <div className="sidebar-foot muted-note">
        Сессии без активности 30 дней удаляются автоматически
      </div>
    </aside>
  );
}

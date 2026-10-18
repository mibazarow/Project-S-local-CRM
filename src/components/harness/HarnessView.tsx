import { useCallback, useEffect, useReducer, useRef, useState, type FormEvent } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import type {
  AgentEventPayload,
  ChatHit,
  MessageAttachmentDto,
  ScheduleDto,
  SessionDto,
} from "../../api/types";
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
} from "../chat/chatShared";
import { useChatAttachments } from "../chat/useChatAttachments";
import TodoPanel from "../chat/TodoPanel";
import { renderSnippet } from "../files/SearchResults";
import { IconClose, IconSearch } from "../files/icons";

const UNTITLED = "Новая сессия";

const KIND_LABEL: Record<string, string> = {
  run_at: "Однократно",
  cron: "Cron",
  notify: "Уведомление",
};

const STATUS_LABEL: Record<string, string> = {
  active: "активно",
  paused: "пауза",
  done: "готово",
  error: "ошибка",
};

const SPEC_PLACEHOLDER: Record<string, string> = {
  run_at: "2026-10-20T10:00:00",
  cron: "0 10 * * *",
  notify: "2026-10-20T10:00:00",
};

const SPEC_HINT: Record<string, string> = {
  run_at: "ISO-время, напр. 2026-10-20T10:00:00",
  cron: "5 полей: минута час день месяц день недели",
  notify: "ISO-время, напр. 2026-10-20T10:00:00",
};

const GROUPS: Array<{ status: ScheduleDto["status"]; title: string }> = [
  { status: "active", title: "Активные" },
  { status: "paused", title: "Пауза" },
  { status: "error", title: "Ошибки" },
  { status: "done", title: "Завершённые" },
];

// sessions.last_active_at arrives as SQLite localtime "YYYY-MM-DD HH:MM:SS".
function formatActivity(value: string): string {
  const d = new Date(value.replace(" ", "T"));
  if (Number.isNaN(d.getTime())) return value;
  const time = d.toLocaleTimeString("ru-RU", { hour: "2-digit", minute: "2-digit" });
  if (d.toDateString() === new Date().toDateString()) return `сегодня ${time}`;
  return `${d.toLocaleDateString("ru-RU", { day: "numeric", month: "short" })} ${time}`;
}

function formatInstant(value: string): string {
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) return value;
  return d.toLocaleString("ru-RU", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });
}

function formatSpec(kind: string, spec: string): string {
  if (kind === "cron") return spec;
  return formatInstant(spec);
}

const PencilIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z" />
  </svg>
);

const TrashIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6" />
  </svg>
);

const RefreshIcon = () => (
  <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M21 12a9 9 0 1 1-2.64-6.36M21 3v6h-6" />
  </svg>
);

function SessionItem({
  session,
  active,
  onOpen,
  onRename,
  onDelete,
}: {
  session: SessionDto;
  active: boolean;
  onOpen: () => void;
  onRename: (id: number, title: string) => void;
  onDelete: () => void;
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
        <span className="name">{session.title ?? UNTITLED}</span>
      )}
      <span className="meta">{formatActivity(session.lastActiveAt)}</span>
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

function ScheduleItem({
  schedule,
  onToggle,
  onDelete,
}: {
  schedule: ScheduleDto;
  onToggle: (s: ScheduleDto) => void;
  onDelete: (s: ScheduleDto) => void;
}) {
  const s = schedule;
  return (
    <div className="schedule-item">
      <div className="info">
        <div className="t">
          {KIND_LABEL[s.kind] ?? s.kind} · {formatSpec(s.kind, s.spec)}
        </div>
        <div className="c">
          {s.timezone ?? "часовой пояс системы"}
          {s.nextRunAt ? ` · след.: ${formatInstant(s.nextRunAt)}` : ""}
        </div>
        {s.error && <div className="c">{s.error}</div>}
      </div>
      <span className={`badge st-${s.status}`}>{STATUS_LABEL[s.status] ?? s.status}</span>
      <label className="switch" title={s.status === "active" ? "Поставить на паузу" : "Возобновить"}>
        <input
          type="checkbox"
          checked={s.status === "active"}
          disabled={s.status === "done"}
          onChange={() => onToggle(s)}
        />
        <span className="track" />
      </label>
      <button className="mini-btn danger" type="button" title="Удалить расписание" onClick={() => onDelete(s)}>
        <TrashIcon />
      </button>
    </div>
  );
}

export default function HarnessView() {
  const { state, actions } = useApp();
  const [sessions, setSessions] = useState<SessionDto[]>([]);
  const [activeId, setActiveId] = useState<number | null>(null);
  const [sessionsError, setSessionsError] = useState<string | null>(null);

  const [query, setQuery] = useState("");
  const [activeQuery, setActiveQuery] = useState("");
  const [hits, setHits] = useState<ChatHit[]>([]);
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [indexNote, setIndexNote] = useState<string | null>(null);

  const [chat, dispatch] = useReducer(chatReducer, {});
  const chatRef = useRef(chat);
  chatRef.current = chat;
  // session id -> chat id, learned from history loads and send returns: the
  // "agent-file" event carries only chatId, so the view maps it back to the
  // session key, and ChatTranscript needs the chat id for agent attachments.
  const [sessionChatIds, setSessionChatIds] = useState<Record<number, number>>({});
  const sessionChatIdsRef = useRef(sessionChatIds);
  sessionChatIdsRef.current = sessionChatIds;
  const rememberChatId = (session: number, chatId: number) =>
    setSessionChatIds((prev) => (prev[session] === chatId ? prev : { ...prev, [session]: chatId }));
  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;
  const [text, setText] = useState("");
  const taRef = useRef<HTMLTextAreaElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const chatRootRef = useRef<HTMLElement | null>(null);
  const att = useChatAttachments(chatRootRef);
  const visionSupported = useVisionSupport();

  const [schedules, setSchedules] = useState<ScheduleDto[]>([]);
  const [schedulesError, setSchedulesError] = useState<string | null>(null);
  const [formOpen, setFormOpen] = useState(false);
  const [kind, setKind] = useState<ScheduleDto["kind"]>("run_at");
  const [spec, setSpec] = useState("");
  const [prompt, setPrompt] = useState("");
  const [timezone, setTimezone] = useState("");
  const [formError, setFormError] = useState<string | null>(null);

  const activeSession = sessions.find((s) => s.id === activeId) ?? null;
  const slice = activeId != null ? (chat[activeId] ?? null) : null;
  const running = slice?.run != null;
  const confirmation = slice?.confirmations[0] ?? null;

  // Sequence guards: overlapping reloads (event bursts plus manual actions)
  // must never let a stale response overwrite a newer one — only the most
  // recent call applies its result.
  const sessionsSeq = useRef(0);
  const schedulesSeq = useRef(0);

  const loadSessions = useCallback(async () => {
    const seq = ++sessionsSeq.current;
    try {
      const list = await ipc.harnessListSessions();
      if (seq !== sessionsSeq.current) return;
      setSessions(list);
      setActiveId((cur) => (cur != null && list.some((s) => s.id === cur) ? cur : (list[0]?.id ?? null)));
      setSessionsError(null);
    } catch (e) {
      if (seq === sessionsSeq.current) setSessionsError(toMessage(e));
    }
  }, []);

  const loadSchedules = useCallback(async () => {
    const seq = ++schedulesSeq.current;
    try {
      const list = await ipc.scheduleList();
      if (seq !== schedulesSeq.current) return;
      setSchedules(list);
      setSchedulesError(null);
    } catch (e) {
      if (seq === schedulesSeq.current) setSchedulesError(toMessage(e));
    }
  }, []);

  // Initial load plus every schedules-changed bump from the store (the App
  // provider subscribes once): a schedule created/changed by a command, an
  // agent tool or a fired tick refreshes the list right away. Sessions reload
  // with it: a create can bind a freshly created session and a fire touches
  // the session's last_active.
  useEffect(() => {
    void loadSessions();
    void loadSchedules();
  }, [loadSessions, loadSchedules, state.schedulesVersion]);

  // Command-palette intent: switch to a session requested before this view
  // mounted. loadSessions keeps an activeId that exists in the list, so the
  // pending id survives the initial load; a vanished id falls back to the
  // first session.
  useEffect(() => {
    const id = state.pendingSessionId;
    if (id == null) return;
    actions.clearPendingSession();
    setActiveId(id);
  }, [state.pendingSessionId, actions]);

  // Chat search: the query is debounced into activeQuery (one request per
  // pause, no polling); an empty query restores the plain session list.
  useEffect(() => {
    const timer = setTimeout(() => setActiveQuery(query.trim()), 250);
    return () => clearTimeout(timer);
  }, [query]);

  useEffect(() => {
    if (!activeQuery) {
      setHits([]);
      setSearchError(null);
      setSearchLoading(false);
      return;
    }
    let cancelled = false;
    setSearchLoading(true);
    setSearchError(null);
    void (async () => {
      try {
        const result = await ipc.searchChats(activeQuery, 10);
        if (!cancelled) setHits(result);
      } catch (e) {
        if (!cancelled) setSearchError(toMessage(e));
      } finally {
        if (!cancelled) setSearchLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [activeQuery]);

  // Chat message index: backfill once on mount (best-effort, silent), on
  // demand via the toolbar button; failures never surface.
  const reindexChats = useCallback((quietOnZero: boolean) => {
    void ipc
      .indexChatMessages()
      .then((n) => {
        if (!quietOnZero || n > 0) setIndexNote(`Индекс обновлён: ${n}`);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    reindexChats(true);
  }, [reindexChats]);

  useEffect(() => {
    if (activeId == null) return;
    if (chatRef.current[activeId]?.loaded) return;
    void ipc
      .harnessSessionHistory(activeId)
      .then((dtos) => {
        const chatId = dtos[0]?.chatId;
        if (chatId != null) rememberChatId(activeId, chatId);
        dispatch({ type: "loaded", key: activeId, items: historyToItems(dtos) });
      })
      .catch(() => dispatch({ type: "loaded", key: activeId, items: [] }));
  }, [activeId]);

  // One shared event stream: harness runs are routed by sessionId. A run
  // started outside this view (a fired schedule) streams live too: the first
  // incoming event opens the run slice (chatReducer), so the open session
  // shows the running state, text and tool steps. done/error also refresh the
  // session list (title / last_active) and, for remote runs, pull the
  // persisted transcript (it carries the schedule's user turn).
  useEffect(() => {
    let cancelled = false;
    let unEvent: (() => void) | undefined;
    let unConfirm: (() => void) | undefined;
    let unSessions: (() => void) | undefined;
    let unFile: (() => void) | undefined;
    const reloadHistory = (key: number) => {
      void ipc
        .harnessSessionHistory(key)
        .then((dtos) => {
          const chatId = dtos[0]?.chatId;
          if (chatId != null) rememberChatId(key, chatId);
          dispatch({ type: "loaded", key, items: historyToItems(dtos) });
        })
        .catch(() => {});
    };
    const onEvent = (p: AgentEventPayload) => {
      if (p.sessionId == null) return; // project-chat events carry projectId
      const key = p.sessionId;
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
          const run = chatRef.current[key]?.run;
          dispatch({
            type: "done",
            key,
            answer: p.event.answer,
            reason: p.event.reason,
            reasoning: p.event.reasoning,
          });
          if (!run || !run.local) {
            if (key === activeIdRef.current) reloadHistory(key);
          }
          // Defensive: the run may have created or touched schedules/tasks;
          // their change events can race the run's done event.
          void loadSessions();
          void loadSchedules();
          break;
        }
        case "error": {
          const run = chatRef.current[key]?.run;
          dispatch({ type: "runError", key, message: p.event.message });
          if (!run || !run.local) {
            if (key === activeIdRef.current) reloadHistory(key);
          }
          void loadSessions();
          void loadSchedules();
          break;
        }
        case "confirmationRequested":
          break; // the id-bearing copy arrives as "agent-confirmation"
      }
    };
    void ipc.onAgentEvent(onEvent).then((u) => (cancelled ? u() : (unEvent = u)));
    void ipc
      .onAgentConfirmation((p) => {
        if (p.sessionId == null) return;
        dispatch({ type: "confirm", key: p.sessionId, payload: p });
      })
      .then((u) => (cancelled ? u() : (unConfirm = u)));
    // Schedules refresh arrives through the store's schedulesVersion (the App
    // provider subscribes to "schedules-changed" once), not a second listener.
    // A fired schedule can create a fresh session (its origin was deleted),
    // and the backend emits this event after a session auto-title or an
    // agent rename_chat: the list (order and titles) refreshes as it changes.
    void ipc.onSessionsChanged(() => void loadSessions()).then((u) => (cancelled ? u() : (unSessions = u)));
    // Agent-sent files: the event carries only chatId, so route it through the
    // remembered session -> chat map; the reducer dedupes by messageId.
    void ipc
      .onAgentFile((p) => {
        const entry = Object.entries(sessionChatIdsRef.current).find(([, chat]) => chat === p.chatId);
        if (!entry) return;
        dispatch({ type: "agentFile", key: Number(entry[0]), payload: p });
      })
      .then((u) => (cancelled ? u() : (unFile = u)));
    return () => {
      cancelled = true;
      unEvent?.();
      unConfirm?.();
      unSessions?.();
      unFile?.();
    };
  }, [loadSessions, loadSchedules]);

  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [chat]);

  const createSession = async () => {
    try {
      const dto = await ipc.harnessCreateSession();
      await loadSessions();
      setActiveId(dto.id);
    } catch (e) {
      setSessionsError(toMessage(e));
    }
  };

  const renameSession = async (id: number, title: string) => {
    try {
      await ipc.harnessRenameSession(id, title);
      await loadSessions();
    } catch (e) {
      setSessionsError(toMessage(e));
    }
  };

  const deleteSession = async (s: SessionDto) => {
    const label = s.title ?? UNTITLED;
    if (!window.confirm(`Удалить сессию «${label}»? Её история будет удалена, а расписания сохранятся и при следующем срабатывании создадут новую сессию.`)) return;
    try {
      await ipc.harnessDeleteSession(s.id);
      if (activeId === s.id) setActiveId(null);
      await loadSessions();
    } catch (e) {
      setSessionsError(toMessage(e));
    }
  };

  const grow = () => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    const cs = getComputedStyle(ta);
    const lh = parseFloat(cs.lineHeight) || parseFloat(cs.fontSize) * 1.5;
    ta.style.height = Math.min(ta.scrollHeight, 10 * lh + 16) + "px";
  };

  const send = () => {
    if (running) return;
    const trimmed = text.trim();
    if (!trimmed) return;
    void (async () => {
      // Auto-create on send: with no active session (empty list or the agent
      // deleted the current one) the first typed message starts a session;
      // the «+» button keeps creating one up front.
      let sessionId = activeId;
      if (sessionId == null) {
        try {
          const dto = await ipc.harnessCreateSession();
          sessionId = dto.id;
          setActiveId(dto.id);
          await loadSessions();
        } catch (e) {
          setSessionsError(toMessage(e));
          return;
        }
      }
      let refs: MessageAttachmentDto[] = [];
      if (att.pending.length > 0) {
        try {
          const saved = await ipc.attachFiles(
            null,
            sessionId,
            att.pending.map((a) => a.path),
          );
          refs = saved.map((a) => ({
            name: a.name,
            savedPath: a.savedPath,
            kind: a.kind,
            isImage: a.isImage,
          }));
        } catch (e) {
          dispatch({ type: "sendFailed", key: sessionId, message: toMessage(e) });
          return;
        }
      }
      setText("");
      grow();
      att.clear();
      dispatch({ type: "send", key: sessionId, text: trimmed, attachments: refs });
      // send_session_message touches the session before it returns: reload
      // the list right after the command lands so the active chat visibly
      // rises to the top immediately, not only when the answer completes.
      // Auto-titles and rename_chat arrive later via "sessions-changed".
      // The command returns the session chat id: remember it for agent-file
      // routing and agent attachment reads before any send_file event lands.
      void ipc
        .sendSessionMessage(sessionId, trimmed, refs)
        .then((chatId) => {
          rememberChatId(sessionId, chatId);
          return loadSessions();
        })
        .catch((e) => dispatch({ type: "sendFailed", key: sessionId, message: toMessage(e) }));
    })();
  };

  const resolve = (decision: "allow" | "deny" | "allowAlways") => {
    if (!confirmation || activeId == null) return;
    void ipc.resolveConfirmation(confirmation.id, decision).catch(() => {});
    dispatch({ type: "confirmResolved", key: activeId, id: confirmation.id });
  };

  const stop = () => {
    if (activeId != null) void ipc.cancelSessionRun(activeId).catch(() => {});
  };

  const toggleSchedule = async (s: ScheduleDto) => {
    try {
      if (s.status === "active") await ipc.schedulePause(s.id);
      else await ipc.scheduleResume(s.id);
      await loadSchedules();
    } catch (e) {
      setSchedulesError(toMessage(e));
    }
  };

  const deleteSchedule = async (s: ScheduleDto) => {
    if (!window.confirm("Удалить расписание?")) return;
    try {
      await ipc.scheduleDelete(s.id);
      await loadSchedules();
    } catch (e) {
      setSchedulesError(toMessage(e));
    }
  };

  const addSchedule = async (e: FormEvent) => {
    e.preventDefault();
    setFormError(null);
    try {
      await ipc.scheduleCreate(
        kind,
        spec.trim(),
        prompt.trim() || null,
        timezone.trim() || null,
        activeId,
      );
      setFormOpen(false);
      setSpec("");
      setPrompt("");
      setTimezone("");
      await loadSchedules();
    } catch (err) {
      setFormError(toMessage(err));
    }
  };

  return (
    <main className="view active">
      <aside className="sidebar">
        <div className="sidebar-head">
          <span className="sidebar-title">Сессии</span>
          <span className="sidebar-head-actions">
            <button
              className="icon-btn small"
              type="button"
              title="Обновить индекс"
              onClick={() => reindexChats(false)}
            >
              <RefreshIcon />
            </button>
            <button className="icon-btn small" type="button" title="Новая сессия" onClick={() => void createSession()}>
              <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><path d="M12 5v14M5 12h14" /></svg>
            </button>
          </span>
        </div>
        <div className="search harness-search">
          <IconSearch />
          <input
            type="text"
            placeholder="Поиск по чатам…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
          {query && (
            <button className="icon-btn small" type="button" title="Очистить поиск" onClick={() => setQuery("")}>
              <IconClose />
            </button>
          )}
        </div>
        {indexNote && <div className="set-hint harness-index-note">{indexNote}</div>}
        {activeQuery ? (
          <div className="session-list harness-search-results">
            {searchLoading && <div className="empty-note">Поиск…</div>}
            {searchError && <div className="tree-error">{searchError}</div>}
            {!searchLoading && !searchError && hits.length === 0 && (
              <div className="empty-note">Ничего не найдено</div>
            )}
            {hits.map((hit) => (
              <div
                className={hit.sessionId === activeId ? "search-hit active" : "search-hit"}
                key={hit.sessionId}
                role="button"
                tabIndex={0}
                onClick={() => setActiveId(hit.sessionId)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") setActiveId(hit.sessionId);
                }}
              >
                <div className="search-hit-head">
                  <span className="search-hit-name">{hit.title ?? UNTITLED}</span>
                  <span className="search-hit-path">{formatActivity(hit.lastActive)}</span>
                  <span className="search-hit-badges">
                    <span className="badge" title="Релевантность совпадения">
                      {hit.score.toFixed(3)}
                    </span>
                  </span>
                </div>
                {hit.snippet && <div className="search-snippet">{renderSnippet(hit.snippet)}</div>}
              </div>
            ))}
          </div>
        ) : (
          <div className="session-list">
            {sessions.map((s) => (
              <SessionItem
                key={s.id}
                session={s}
                active={s.id === activeId}
                onOpen={() => setActiveId(s.id)}
                onRename={(id, title) => void renameSession(id, title)}
                onDelete={() => void deleteSession(s)}
              />
            ))}
            {sessions.length === 0 && (
              <div className="empty-note">
                Сессий пока нет. Напишите сообщение или нажмите «+», чтобы создать первую.
              </div>
            )}
          </div>
        )}
        <div className="sidebar-foot muted-note">
          Сессии без активности 30 дней удаляются автоматически
        </div>
      </aside>

      <section
        className={att.dragOver ? "center center-harness drag-over" : "center center-harness"}
        ref={chatRootRef}
        data-chat-drop
      >
        <div className="center-head">
          <h2>{activeSession ? activeSession.title ?? UNTITLED : "Харнес"}</h2>
          {running && <span className="chat-sub">агент работает…</span>}
        </div>

        <TodoPanel scopeKind="session" scopeId={activeId} />

        <ChatTranscript
          slice={slice}
          listRef={listRef}
          className="chat-messages harness-messages"
          header={sessionsError ? <div className="chat-error">{sessionsError}</div> : undefined}
          empty={
            sessions.length === 0 && !sessionsError ? (
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
          chatId={activeId != null ? (sessionChatIds[activeId] ?? null) : null}
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
            placeholder="Поручение общему агенту…"
            value={text}
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
      </section>

      <aside className="chat">
        <div className="chat-head">
          <span className="chat-title">Расписания</span>
          <button
            className="icon-btn small"
            type="button"
            title={formOpen ? "Закрыть форму" : "Добавить расписание"}
            onClick={() => {
              setFormOpen((v) => !v);
              setFormError(null);
            }}
          >
            <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><path d="M12 5v14M5 12h14" /></svg>
          </button>
        </div>

        {formOpen && (
          <form className="schedule-form" onSubmit={(e) => void addSchedule(e)}>
            <div className="set-row">
              <label>Тип</label>
              <select value={kind} onChange={(e) => setKind(e.target.value as ScheduleDto["kind"])}>
                <option value="run_at">Однократно (run_at)</option>
                <option value="cron">Повтор по расписанию (cron)</option>
                <option value="notify">Уведомление (notify)</option>
              </select>
            </div>
            <div className="set-row">
              <label>Расписание</label>
              <input
                value={spec}
                onChange={(e) => setSpec(e.target.value)}
                placeholder={SPEC_PLACEHOLDER[kind]}
                required
              />
              <span className="set-hint">{SPEC_HINT[kind]}</span>
            </div>
            <div className="set-row">
              <label>{kind === "notify" ? "Текст уведомления" : "Поручение агенту"}</label>
              <textarea
                rows={2}
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                placeholder={kind === "notify" ? "Напомнить о…" : "Что сделать агенту…"}
              />
            </div>
            <div className="set-row">
              <label>Часовой пояс (необязательно)</label>
              <input value={timezone} onChange={(e) => setTimezone(e.target.value)} placeholder="Europe/Moscow" />
            </div>
            <span className="set-hint">
              {activeSession
                ? `run_at и cron запустятся в сессии «${activeSession.title ?? UNTITLED}»`
                : "Откройте сессию — run_at и cron запустятся в ней"}
            </span>
            {formError && <span className="set-error">{formError}</span>}
            <div className="form-actions">
              <button className="primary-btn small" type="submit">
                Добавить
              </button>
              <button className="ghost-btn small" type="button" onClick={() => setFormOpen(false)}>
                Отмена
              </button>
            </div>
          </form>
        )}

        <div className="schedule-list">
          {schedulesError && <div className="set-error">{schedulesError}</div>}
          {schedules.length === 0 && !schedulesError && (
            <div className="empty-note">Расписаний пока нет. Нажмите «+», чтобы добавить.</div>
          )}
          {GROUPS.map((g) => {
            const items = schedules.filter((s) => s.status === g.status);
            if (items.length === 0) return null;
            return (
              <div key={g.status} className="schedule-group">
                <div className="schedule-group-title">{g.title}</div>
                {items.map((s) => (
                  <ScheduleItem
                    key={s.id}
                    schedule={s}
                    onToggle={(sch) => void toggleSchedule(sch)}
                    onDelete={(sch) => void deleteSchedule(sch)}
                  />
                ))}
              </div>
            );
          })}
        </div>
      </aside>
    </main>
  );
}

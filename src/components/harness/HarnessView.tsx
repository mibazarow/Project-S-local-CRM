import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import { useSearchHits } from "../../hooks/useSearchHits";
import type {
  AgentEventPayload,
  MessageAttachmentDto,
  ScheduleDto,
  SessionDto,
} from "../../api/types";
import {
  historyToItems,
  toMessage,
  ChatTranscript,
  ChatHeaderTools,
  ConfirmCard,
  AttachFileIcon,
  AttachXIcon,
  countMatches,
  downloadMarkdown,
  resolvePinMessageId,
  resolveRegenerateTurn,
  runChatExport,
  useVisionSupport,
  type ChatItem,
} from "../chat/chatShared";
import { useChatRun } from "../chat/useChatRun";
import { ChatComposer } from "../chat/ChatComposer";
import { useChatAttachments } from "../chat/useChatAttachments";
import { formatActivity } from "../../shared/format";
import TodoPanel from "../chat/TodoPanel";
import { renderSnippet } from "../files/SearchResults";
import { IconClose, IconSearch } from "../files/icons";
import SchedulePanel from "./SchedulePanel";
import { TrashIcon } from "./shared";

const UNTITLED = "Новая сессия";

const PencilIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z" />
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

export default function HarnessView() {
  const { state, actions } = useApp();
  const [sessions, setSessions] = useState<SessionDto[]>([]);
  const [activeId, setActiveId] = useState<number | null>(null);
  const [sessionsError, setSessionsError] = useState<string | null>(null);

  const [query, setQuery] = useState("");
  const [indexNote, setIndexNote] = useState<string | null>(null);

  // session id -> chat id, learned from history loads and send returns: the
  // "agent-file" event carries only chatId, so the view maps it back to the
  // session key, and ChatTranscript needs the chat id for agent attachments.
  const [sessionChatIds, setSessionChatIds] = useState<Record<number, number>>({});
  const sessionChatIdsRef = useRef(sessionChatIds);
  sessionChatIdsRef.current = sessionChatIds;
  const rememberChatId = useCallback(
    (session: number, chatId: number) =>
      setSessionChatIds((prev) => (prev[session] === chatId ? prev : { ...prev, [session]: chatId })),
    [],
  );
  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;
  const [text, setText] = useState("");
  const [chatQuery, setChatQuery] = useState("");
  const [branchNote, setBranchNote] = useState<string | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const chatRootRef = useRef<HTMLElement | null>(null);
  const att = useChatAttachments(chatRootRef);
  const visionSupported = useVisionSupport();

  const [schedules, setSchedules] = useState<ScheduleDto[]>([]);
  const [schedulesError, setSchedulesError] = useState<string | null>(null);

  const activeSession = sessions.find((s) => s.id === activeId) ?? null;

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

  const reloadHistory = (key: number, dispatch: ReturnType<typeof useChatRun>["dispatch"]) => {
    void ipc
      .harnessSessionHistory(key)
      .then((dtos) => {
        const chatId = dtos[0]?.chatId;
        if (chatId != null) rememberChatId(key, chatId);
        dispatch({ type: "loaded", key, items: historyToItems(dtos) });
      })
      .catch(() => {});
  };

  // Harness sessions key by sessionId; agent-file/compaction events carry only
  // a chatId, so they route back through the remembered session -> chat map. A
  // run started outside this view (a fired schedule) streams live too; its
  // done/error pulls the persisted transcript for the open session and refreshes
  // the session and schedule lists (titles / last_active / touched schedules).
  const { state: chat, stateRef, dispatch } = useChatRun(
    {
      routeAgentEvent: (p: AgentEventPayload) => (p.sessionId == null ? null : p.sessionId),
      routeConfirmation: (p) => (p.sessionId == null ? null : p.sessionId),
      routeAgentFile: (p) => {
        const entry = Object.entries(sessionChatIdsRef.current).find(([, chat]) => chat === p.chatId);
        return entry ? Number(entry[0]) : null;
      },
      routeSkillProposal: (p) => {
        const entry = Object.entries(sessionChatIdsRef.current).find(([, chat]) => chat === p.chatId);
        return entry ? Number(entry[0]) : null;
      },
      routeCompaction: (p) => {
        const entry = Object.entries(sessionChatIdsRef.current).find(([, chat]) => chat === p.chatId);
        return entry ? Number(entry[0]) : null;
      },
      onRunEnd: (key, run, dispatch) => {
        if (!run || !run.local) {
          if (key === activeIdRef.current) reloadHistory(key, dispatch);
        }
        void loadSessions();
        void loadSchedules();
      },
      sendQueued: (key, combined, dispatch) => {
        void ipc
          .sendSessionMessage(key, combined)
          .then((chatId) => {
            rememberChatId(key, chatId);
            return loadSessions();
          })
          .catch((e) => dispatch({ type: "sendFailed", key, message: toMessage(e) }));
      },
    },
    [loadSessions, loadSchedules],
  );

  const slice = activeId != null ? (chat[activeId] ?? null) : null;
  const running = slice?.run != null;
  const confirmation = slice?.confirmations[0] ?? null;

  // The session list can change without a run ending in this view: the session
  // auto-title, rename_chat, chat_branch and a fired schedule creating a fresh
  // session all emit "sessions-changed".
  useEffect(() => {
    let cancelled = false;
    let unSessions: (() => void) | undefined;
    void ipc
      .onSessionsChanged(() => void loadSessions())
      .then((u) => (cancelled ? u() : (unSessions = u)));
    return () => {
      cancelled = true;
      unSessions?.();
    };
  }, [loadSessions]);

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
  const activeQuery = useDebouncedValue(query.trim(), 250);
  const {
    hits,
    loading: searchLoading,
    error: searchError,
  } = useSearchHits(() => ipc.searchChats(activeQuery, 10), [activeQuery], {
    skip: !activeQuery,
  });

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
    if (stateRef.current[activeId]?.loaded) return;
    void ipc
      .harnessSessionHistory(activeId)
      .then((dtos) => {
        const chatId = dtos[0]?.chatId;
        if (chatId != null) rememberChatId(activeId, chatId);
        dispatch({ type: "loaded", key: activeId, items: historyToItems(dtos) });
      })
      .catch(() => dispatch({ type: "loaded", key: activeId, items: [] }));
  }, [activeId]);

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

  const sendText = (value: string) => {
    if (running) return;
    const trimmed = value.trim();
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

  // While a run is live the composer stays enabled: sending queues the text
  // (flushed as one combined turn when the run finishes) instead of starting
  // a second run.
  const send = () => {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (running && activeId != null) {
      dispatch({ type: "enqueue", key: activeId, text: trimmed });
      setText("");
      return;
    }
    sendText(text);
  };

  const resolve = (decision: "allow" | "deny" | "allowAlways") => {
    if (!confirmation || activeId == null) return;
    void ipc.resolveConfirmation(confirmation.id, decision).catch(() => {});
    dispatch({ type: "confirmResolved", key: activeId, id: confirmation.id });
  };

  const stop = () => {
    if (activeId != null) void ipc.cancelSessionRun(activeId).catch(() => {});
  };

  // Message-level UX: live items carry no DB ids, so the handlers refetch the
  // history and map the item by position before touching the transcript.
  const pinItem = useCallback(
    async (item: ChatItem, pinned: boolean) => {
      if (activeId == null) return;
      const chatId = sessionChatIdsRef.current[activeId];
      if (chatId == null) return;
      const messageId = await resolvePinMessageId(
        () => ipc.harnessSessionHistory(activeId),
        stateRef.current[activeId]?.items ?? [],
        item.key,
      );
      if (!messageId) return;
      try {
        await ipc.chatPin(chatId, messageId, pinned);
        dispatch({ type: "pin", key: activeId, itemKey: item.key, pinned });
      } catch {
        // The pin flag simply stays as it was.
      }
    },
    [activeId, stateRef, dispatch],
  );

  const regenerateItem = useCallback(
    async (item: ChatItem) => {
      if (activeId == null) return;
      const sliceNow = stateRef.current[activeId];
      if (sliceNow?.run != null) return;
      const chatId = sessionChatIdsRef.current[activeId];
      if (chatId == null) return;
      const turn = await resolveRegenerateTurn(
        () => ipc.harnessSessionHistory(activeId),
        sliceNow?.items ?? [],
        item.key,
      );
      if (!turn) return;
      try {
        await ipc.chatTruncateAfter(chatId, turn.messageId);
        dispatch({ type: "regenerate", key: activeId, itemKey: item.key });
        await ipc
          .sendSessionMessage(activeId, turn.userText)
          .then((chat) => {
            rememberChatId(activeId, chat);
            return loadSessions();
          })
          .catch((e) => dispatch({ type: "sendFailed", key: activeId, message: toMessage(e) }));
      } catch (e) {
        dispatch({ type: "sendFailed", key: activeId, message: toMessage(e) });
      }
    },
    [activeId, stateRef, dispatch, rememberChatId, loadSessions],
  );

  // Branching forks the session at a message: the fresh session opens through
  // the pendingSessionId intent, and chat_branch's "sessions-changed" event
  // refreshes the sidebar list.
  const branchItem = useCallback(
    async (item: ChatItem) => {
      if (activeId == null) return;
      const chatId = sessionChatIdsRef.current[activeId];
      if (chatId == null) return;
      const messageId = await resolvePinMessageId(
        () => ipc.harnessSessionHistory(activeId),
        stateRef.current[activeId]?.items ?? [],
        item.key,
      );
      if (!messageId) return;
      try {
        const res = await ipc.chatBranch(chatId, messageId);
        rememberChatId(res.sessionId, res.chatId);
        actions.requestSession(res.sessionId);
        setBranchNote("Ветка создана — открыта новая сессия.");
      } catch (e) {
        setBranchNote(toMessage(e));
      }
    },
    [activeId, stateRef, rememberChatId, actions],
  );

  const transcriptActions = useMemo(
    () => ({ onPin: pinItem, onRegenerate: regenerateItem, onBranch: branchItem }),
    [pinItem, regenerateItem, branchItem],
  );

  const exportChat = () => {
    if (activeId == null) return;
    const chatId = sessionChatIds[activeId];
    if (chatId == null) return;
    runChatExport(chatId, (r) => downloadMarkdown(r.markdown, `session-chat-${chatId}.md`));
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

  // Click-through: open the schedule's chat (the backend reuses its session
  // or creates one titled after the schedule) and ask the agent to explain
  // the task there — the question and the answer are visible in the chat.
  const openSchedule = async (s: ScheduleDto) => {
    try {
      const { sessionId } = await ipc.scheduleOpen(s.id);
      setActiveId(sessionId);
      await loadSessions();
      if (stateRef.current[sessionId]?.run) return; // a run is already live there
      const question = `Коротко (1–2 предложения) объясни, что это за задача по расписанию: «${s.title ?? s.spec}» (id ${s.id}).`;
      dispatch({ type: "send", key: sessionId, text: question, attachments: [] });
      void ipc
        .sendSessionMessage(sessionId, question)
        .then((chatId) => {
          rememberChatId(sessionId, chatId);
          return loadSessions();
        })
        .catch((e) => dispatch({ type: "sendFailed", key: sessionId, message: toMessage(e) }));
    } catch (e) {
      setSchedulesError(toMessage(e));
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
          <ChatHeaderTools
            query={chatQuery}
            onQuery={setChatQuery}
            matchCount={countMatches(slice?.items ?? [], chatQuery)}
            onExport={exportChat}
            exportDisabled={activeId == null || (activeId != null && sessionChatIds[activeId] == null)}
          />
        </div>

        {branchNote && <div className="set-hint harness-branch-note">{branchNote}</div>}

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
          actions={transcriptActions}
          search={chatQuery}
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

        <ChatComposer
          text={text}
          onTextChange={setText}
          onSend={send}
          onStop={stop}
          onClear={() => {
            setText("");
            att.clear();
          }}
          running={running}
          queueCount={slice?.queue.length ?? 0}
          placeholder="Поручение общему агенту…"
          attach={{ onPick: () => att.pick().catch(() => {}) }}
        />
      </section>

      <SchedulePanel
        schedules={schedules}
        schedulesError={schedulesError}
        activeId={activeId}
        activeSessionTitle={activeSession ? activeSession.title ?? UNTITLED : null}
        onToggle={(s) => void toggleSchedule(s)}
        onDelete={(s) => void deleteSchedule(s)}
        onOpen={(s) => void openSchedule(s)}
        onReload={loadSchedules}
      />
    </main>
  );
}

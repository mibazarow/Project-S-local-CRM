import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import type { MessageAttachmentDto, ScheduleDto, SessionDto } from "../../api/types";
import {
  historyToItems,
  toMessage,
  ChatTranscript,
  ConfirmCard,
  AttachFileIcon,
  AttachXIcon,
  resolvePinMessageId,
  resolveRegenerateTurn,
  useVisionSupport,
  type ChatItem,
} from "../chat/chatShared";
import {
  dispatch,
  ensureSubscribed,
  getSlice,
  openChat,
  closeChat,
  onRunEnd,
  onSent,
  rememberChatId as storeRememberChatId,
  send as storeSend,
  sessionKey,
  useChatSnapshot,
} from "../../state/chatRunStore";
import { ChatComposer } from "../chat/ChatComposer";
import { useChatAttachments } from "../chat/useChatAttachments";
import { formatActivity } from "../../shared/format";
import { confirmRemove } from "../../shared/confirm";
import TodoPanel from "../chat/TodoPanel";
import SchedulePanel from "./SchedulePanel";
import { TrashIcon } from "./shared";
import { IconPin } from "../files/icons";

const UNTITLED = "Новая сессия";

function pluralRu(n: number, one: string, few: string, many: string): string {
  const mod10 = n % 10;
  const mod100 = n % 100;
  if (mod10 === 1 && mod100 !== 11) return one;
  if (mod10 >= 2 && mod10 <= 4 && (mod100 < 10 || mod100 >= 20)) return few;
  return many;
}

// The schedules panel «+»: a dedicated Harness chat seeded with a static
// assistant greeting (persisted by the backend, no LLM call) — the schedule
// itself is then created by the agent's schedule tools from the user's words.
const SCHEDULE_CHAT_TITLE = "Новое расписание";
const SCHEDULE_CHAT_GREETING =
  "Помогу создать расписание. Что нужно делать, в какое время и как часто — разово или регулярно?";

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

export default function HarnessView() {
  const { state, actions } = useApp();
  const [sessions, setSessions] = useState<SessionDto[]>([]);
  const [activeId, setActiveId] = useState<number | null>(null);
  const [sessionsError, setSessionsError] = useState<string | null>(null);

  const [indexNote, setIndexNote] = useState<string | null>(null);

  // session id -> chat id, learned from history loads and send returns: the
  // "agent-file" event carries only chatId, so the view maps it back to the
  // session key, and ChatTranscript needs the chat id for agent attachments.
  const [sessionChatIds, setSessionChatIds] = useState<Record<number, number>>({});
  const sessionChatIdsRef = useRef(sessionChatIds);
  sessionChatIdsRef.current = sessionChatIds;
  const rememberChatId = useCallback(
    (session: number, chatId: number) => {
      storeRememberChatId(sessionKey(session), chatId);
      setSessionChatIds((prev) => (prev[session] === chatId ? prev : { ...prev, [session]: chatId }));
    },
    [],
  );
  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;
  const [text, setText] = useState("");
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

  const reloadHistory = (sessionId: number) => {
    const key = sessionKey(sessionId);
    void ipc
      .harnessSessionHistory(sessionId)
      .then((dtos) => {
        const chatId = dtos[0]?.chatId;
        if (chatId != null) rememberChatId(sessionId, chatId);
        dispatch({ type: "loaded", key, items: historyToItems(dtos) });
      })
      .catch(() => {});
  };

  // Harness sessions key by sessionId in the global store; the store owns the
  // five backend subscriptions and event routing, so a run keeps streaming when
  // this view unmounts and several sessions run in parallel. A run started
  // outside this view (a fired schedule) streams live too; its done/error pulls
  // the persisted transcript for the open session and refreshes the lists.
  const chat = useChatSnapshot();
  const key = activeId != null ? sessionKey(activeId) : "";

  useEffect(() => {
    ensureSubscribed();
  }, []);

  useEffect(() => {
    if (key === "") return;
    openChat(key);
    return () => closeChat(key);
  }, [key]);

  useEffect(
    () =>
      onRunEnd((endedKey, wasLocal) => {
        if (!wasLocal) {
          const sessionId = Number(endedKey.slice(2));
          if (endedKey === sessionKey(activeIdRef.current ?? -1)) reloadHistory(sessionId);
        }
        void loadSessions();
        void loadSchedules();
      }),
    // loadSessions/loadSchedules are stable; reloadHistory reads current refs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [loadSessions, loadSchedules],
  );

  // A send started from this view touches the session: reload the list right
  // after the command lands so the active chat rises to the top immediately.
  useEffect(() => onSent(() => void loadSessions()), [loadSessions]);

  const slice = activeId != null ? (chat[key] ?? null) : null;
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
    const k = sessionKey(activeId);
    if (getSlice(k)?.loaded) return;
    void ipc
      .harnessSessionHistory(activeId)
      .then((dtos) => {
        const chatId = dtos[0]?.chatId;
        if (chatId != null) rememberChatId(activeId, chatId);
        dispatch({ type: "loaded", key: k, items: historyToItems(dtos) });
      })
      .catch(() => dispatch({ type: "loaded", key: k, items: [] }));
  }, [activeId, rememberChatId]);

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

  const createScheduleChat = async () => {
    try {
      const dto = await ipc.harnessCreateSession(SCHEDULE_CHAT_TITLE, SCHEDULE_CHAT_GREETING);
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

  const togglePin = async (s: SessionDto) => {
    try {
      await ipc.harnessPinSession(s.id, !s.pinned);
      await loadSessions();
    } catch (e) {
      setSessionsError(toMessage(e));
    }
  };

  const deleteSession = async (s: SessionDto) => {
    const label = s.title ?? UNTITLED;
    const linked = schedules.filter((sc) => sc.sessionId === s.id).length;
    const message =
      linked > 0
        ? `Чат и ${linked} ${pluralRu(linked, "расписание", "расписания", "расписаний")} будут удалены.`
        : "История чата будет удалена.";
    const ok = await confirmRemove({ title: `Удалить чат «${label}»?`, message });
    if (!ok) return;
    try {
      await ipc.harnessDeleteSession(s.id);
      if (activeId === s.id) setActiveId(null);
      await loadSessions();
      await loadSchedules();
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
          dispatch({ type: "sendFailed", key: sessionKey(sessionId), message: toMessage(e) });
          return;
        }
      }
      setText("");
      att.clear();
      // storeSend opens the run, issues send_session_message and remembers the
      // returned chat id; the onSent subscription reloads the session list so
      // the active chat rises to the top immediately. Auto-titles and
      // rename_chat arrive later via "sessions-changed".
      storeSend(sessionKey(sessionId), trimmed, refs);
    })();
  };

  // While a run is live the composer stays enabled: sending queues the text
  // (flushed as one combined turn when the run finishes) instead of starting
  // a second run.
  const send = () => {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (running && activeId != null) {
      dispatch({ type: "enqueue", key: sessionKey(activeId), text: trimmed });
      setText("");
      return;
    }
    sendText(text);
  };

  const resolve = (decision: "allow" | "deny" | "allowAlways") => {
    if (!confirmation || activeId == null) return;
    void ipc.resolveConfirmation(confirmation.id, decision).catch(() => {});
    dispatch({ type: "confirmResolved", key: sessionKey(activeId), id: confirmation.id });
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
        getSlice(sessionKey(activeId))?.items ?? [],
        item.key,
      );
      if (!messageId) return;
      try {
        await ipc.chatPin(chatId, messageId, pinned);
        dispatch({ type: "pin", key: sessionKey(activeId), itemKey: item.key, pinned });
      } catch {
        // The pin flag simply stays as it was.
      }
    },
    [activeId, dispatch],
  );

  const regenerateItem = useCallback(
    async (item: ChatItem) => {
      if (activeId == null) return;
      const k = sessionKey(activeId);
      const sliceNow = getSlice(k);
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
        dispatch({ type: "regenerate", key: k, itemKey: item.key });
        void ipc
          .sendSessionMessage(activeId, turn.userText)
          .then((chat) => rememberChatId(activeId, chat))
          .catch((e) => dispatch({ type: "sendFailed", key: k, message: toMessage(e) }));
      } catch (e) {
        dispatch({ type: "sendFailed", key: k, message: toMessage(e) });
      }
    },
    [activeId, dispatch, rememberChatId],
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
        getSlice(sessionKey(activeId))?.items ?? [],
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
    [activeId, rememberChatId, actions],
  );

  const transcriptActions = useMemo(
    () => ({ onPin: pinItem, onRegenerate: regenerateItem, onRetry: regenerateItem, onBranch: branchItem }),
    [pinItem, regenerateItem, branchItem],
  );

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
    const label = s.title ?? s.spec;
    const message =
      s.sessionId != null
        ? `Расписание «${label}» и его чат будут удалены.`
        : "Расписание будет удалено.";
    if (!(await confirmRemove({ title: "Удалить расписание?", message }))) return;
    try {
      await ipc.scheduleDelete(s.id);
      await loadSchedules();
      await loadSessions();
    } catch (e) {
      setSchedulesError(toMessage(e));
    }
  };

  // Click-through: open the schedule's chat (the backend reuses its session or
  // creates one titled after the schedule). The chat's own history is loaded and
  // shown first; the explanatory auto-question is sent only for a genuinely
  // empty/new session, so opening a schedule never hides its earlier messages.
  const openSchedule = async (s: ScheduleDto) => {
    try {
      const { sessionId } = await ipc.scheduleOpen(s.id);
      setActiveId(sessionId);
      await loadSessions();
      const k = sessionKey(sessionId);
      if (getSlice(k)?.run) return; // a run is already live there
      const dtos = await ipc.harnessSessionHistory(sessionId).catch(() => []);
      const firstChatId = dtos[0]?.chatId;
      if (firstChatId != null) rememberChatId(sessionId, firstChatId);
      const items = historyToItems(dtos);
      dispatch({ type: "loaded", key: k, items });
      if (items.length > 0) return;
      const question = `Коротко (1–2 предложения) объясни, что это за задача по расписанию: «${s.title ?? s.spec}» (id ${s.id}).`;
      storeSend(k, question, []);
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
            <button className="new-chat-btn" type="button" title="Новая сессия" onClick={() => void createSession()}>
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
              onOpen={() => setActiveId(s.id)}
              onRename={(id, title) => void renameSession(id, title)}
              onDelete={() => void deleteSession(s)}
              onTogglePin={() => void togglePin(s)}
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
        onToggle={(s) => void toggleSchedule(s)}
        onDelete={(s) => void deleteSchedule(s)}
        onOpen={(s) => void openSchedule(s)}
        onCreate={() => void createScheduleChat()}
      />
    </main>
  );
}

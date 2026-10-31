import { useCallback, useEffect, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import type { MessageAttachmentDto, SessionDto } from "../../api/types";
import { toMessage } from "../../shared/errors";
import { historyToItems } from "../chat/chatHistory";
import { useChatAttachments } from "../chat/attachments/useChatAttachments";
import { useVisionSupport } from "../chat/composer/useVisionSupport";
import {
  dispatch,
  ensureSubscribed,
  getSlice,
  openChat,
  closeChat,
  onRunEnd,
  onSent,
  rememberChatId as storeRememberChatId,
  requestStop,
  send as storeSend,
  sessionKey,
  useChatSnapshot,
} from "../../state/chatRunStore";
import SchedulePanel from "./SchedulePanel";
import { SessionsSidebar } from "./SessionsSidebar";
import { HarnessChatPanel } from "./HarnessChatPanel";
import { useHarnessLists } from "./useHarnessLists";
import { useSessionChatActions } from "./useSessionChatActions";

export default function HarnessView() {
  const { state, actions } = useApp();
  const [activeId, setActiveId] = useState<number | null>(null);

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

  const {
    sessions,
    sessionsError,
    setSessionsError,
    schedules,
    schedulesError,
    loadSessions,
    loadSchedules,
    createSession,
    createScheduleChat,
    renameSession,
    togglePin,
    setWorkingDir,
    deleteSession,
    toggleSchedule,
    deleteSchedule,
    openSchedule,
  } = useHarnessLists({ activeId, setActiveId, rememberChatId });

  const activeSession = sessions.find((s) => s.id === activeId) ?? null;

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
    if (activeId != null) requestStop(sessionKey(activeId));
  };

  const transcriptActions = useSessionChatActions({
    activeId,
    sessionChatIdsRef,
    rememberChatId,
    requestSession: actions.requestSession,
    setBranchNote,
  });

  return (
    <main className="view active">
      <SessionsSidebar
        sessions={sessions}
        activeId={activeId}
        chat={chat}
        indexNote={indexNote}
        onReindex={() => reindexChats(false)}
        onCreate={() => void createSession()}
        onOpen={setActiveId}
        onRename={(id, title) => void renameSession(id, title)}
        onDelete={(s: SessionDto) => void deleteSession(s)}
        onTogglePin={(s: SessionDto) => void togglePin(s)}
      />

      <HarnessChatPanel
        att={att}
        chatRootRef={chatRootRef}
        listRef={listRef}
        activeSession={activeSession}
        activeId={activeId}
        chatId={activeId != null ? (sessionChatIds[activeId] ?? null) : null}
        slice={slice}
        running={running}
        hasSessions={sessions.length > 0}
        sessionsError={sessionsError}
        branchNote={branchNote}
        confirmation={confirmation}
        transcriptActions={transcriptActions}
        visionSupported={visionSupported}
        text={text}
        onTextChange={setText}
        onSend={send}
        onStop={stop}
        onClear={() => {
          setText("");
          att.clear();
        }}
        onResolve={resolve}
        onSetWorkingDir={(path) => {
          if (activeSession) void setWorkingDir(activeSession, path);
        }}
      />

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

import { useCallback, useRef, useState, type Dispatch, type SetStateAction } from "react";
import * as ipc from "../../api/ipc";
import type { ScheduleDto, SessionDto } from "../../api/types";
import { toMessage } from "../../shared/errors";
import { confirmRemove } from "../../shared/confirm";
import { historyToItems } from "../chat/chatHistory";
import { dispatch, getSlice, send as storeSend, sessionKey } from "../../state/chatRunStore";
import { pluralRu, UNTITLED } from "./shared";

// The schedules panel «+»: a dedicated Harness chat seeded with a static
// assistant greeting (persisted by the backend, no LLM call) — the schedule
// itself is then created by the agent's schedule tools from the user's words.
const SCHEDULE_CHAT_TITLE = "Новое расписание";
const SCHEDULE_CHAT_GREETING =
  "Помогу создать расписание. Что нужно делать, в какое время и как часто — разово или регулярно?";

// The harness sidebar lists: sessions and schedules, their loads and every
// mutation the sidebar/panel controls issue. Sequence guards: overlapping
// reloads (event bursts plus manual actions) must never let a stale response
// overwrite a newer one — only the most recent call applies its result.
export function useHarnessLists({
  activeId,
  setActiveId,
  rememberChatId,
}: {
  activeId: number | null;
  setActiveId: Dispatch<SetStateAction<number | null>>;
  rememberChatId: (session: number, chatId: number) => void;
}) {
  const [sessions, setSessions] = useState<SessionDto[]>([]);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [schedules, setSchedules] = useState<ScheduleDto[]>([]);
  const [schedulesError, setSchedulesError] = useState<string | null>(null);
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
  }, [setActiveId]);

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

  // Q.md items 2/3/6: click-through opens the schedule's chat (the backend
  // reuses its session or creates one titled after the schedule, clearing the
  // unread highlight). The chat's full history is loaded and shown first. An
  // unread schedule (unreadAt set) only shows its result — no prompt is sent.
  // A read schedule gets the short titled explanation prompt, queued through
  // the run store while a run is live there.
  const openSchedule = async (s: ScheduleDto) => {
    try {
      const { sessionId } = await ipc.scheduleOpen(s.id);
      setActiveId(sessionId);
      await loadSessions();
      const k = sessionKey(sessionId);
      const dtos = await ipc.harnessSessionHistory(sessionId).catch(() => []);
      const firstChatId = dtos[0]?.chatId;
      if (firstChatId != null) rememberChatId(sessionId, firstChatId);
      dispatch({ type: "loaded", key: k, items: historyToItems(dtos) });
      if (s.unreadAt != null) return;
      const label = s.title || s.spec;
      const question =
        `Кратко расскажи, что это за расписание: «${label}». ` +
        "Ответ — 1–2 предложения, без списков и подробных пояснений.";
      if (getSlice(k)?.run) dispatch({ type: "enqueue", key: k, text: question });
      else storeSend(k, question, []);
    } catch (e) {
      setSchedulesError(toMessage(e));
    }
  };

  return {
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
    deleteSession,
    toggleSchedule,
    deleteSchedule,
    openSchedule,
  };
}

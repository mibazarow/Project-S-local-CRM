import { useCallback, useEffect, useReducer, useRef, type Dispatch } from "react";
import * as ipc from "../../api/ipc";
import type {
  AgentCompactionEvent,
  AgentEventPayload,
  AgentFileEvent,
  ConfirmationRequestPayload,
  SkillProposalEvent,
} from "../../api/types";
import {
  chatReducer,
  combineQueued,
  summarizeArgs,
  type ChatAction,
  type ChatState,
  type Run,
} from "./chatShared";

// Shared chat-run plumbing for the three chats (project / notes / harness):
// the transcript reducer, the five backend subscriptions (agent-event,
// agent-confirmation, agent-file, agent-context-compacted,
// agent-skill-proposal), confirmation bookkeeping and the per-chat message
// queue + its flush timing. The three views differ only in how an event maps
// to a slice key and what side effects a finished run triggers, so those are
// injected through the config; the event order, the reducer actions and the
// 400 ms flush delay are identical here.

export interface ChatRunConfig {
  // Map a payload to its slice key, or null to ignore it (the per-view routing:
  // project chats key by projectId, harness by sessionId, notes by the
  // notes:true flag / the singleton chatId; agent-file and compaction events
  // carry only chatId, so the view resolves it through its own chat map).
  routeAgentEvent: (p: AgentEventPayload) => number | null;
  routeConfirmation: (p: ConfirmationRequestPayload) => number | null;
  routeAgentFile: (p: AgentFileEvent) => number | null;
  routeSkillProposal: (p: SkillProposalEvent) => number | null;
  routeCompaction: (p: AgentCompactionEvent) => number | null;
  // Fires right after the done/runError dispatch and before the queue flush.
  // `run` is the slice's run as it was before the dispatch (null when none),
  // so a view can tell a local run from a remote (fired-schedule) one and pull
  // the persisted transcript for the latter.
  onRunEnd?: (key: number, run: Run | null, dispatch: Dispatch<ChatAction>) => void;
  // Flush the combined queue turn to the backend once the run is idle. The
  // reducer "send" already opened the run; this only issues the IPC command and
  // its per-view follow-ups (remember the chat id, reload the session list).
  sendQueued: (key: number, combined: string, dispatch: Dispatch<ChatAction>) => void;
}

export interface UseChatRun {
  state: ChatState;
  stateRef: React.RefObject<ChatState>;
  dispatch: Dispatch<ChatAction>;
}

export function useChatRun(config: ChatRunConfig, subscribeDeps: unknown[]): UseChatRun {
  const [state, dispatch] = useReducer(chatReducer, {});
  const stateRef = useRef(state);
  stateRef.current = state;

  // Always-current config so the subscriptions (which run once per
  // subscribeDeps) never close over a stale routing/side-effect closure.
  const configRef = useRef(config);
  configRef.current = config;

  // When a run finishes (done/error/cancelled), flush the queued messages as
  // one combined user turn. A short delay lets the finished run persist its
  // final answer before the new run's user turn is appended; if another run
  // started meanwhile the text is re-queued for its end.
  const flushQueue = useCallback((key: number) => {
    const queued = stateRef.current[key]?.queue ?? [];
    if (queued.length === 0) return;
    dispatch({ type: "clearQueue", key });
    const combined = combineQueued(queued);
    setTimeout(() => {
      if (stateRef.current[key]?.run) {
        dispatch({ type: "enqueue", key, text: combined });
        return;
      }
      dispatch({ type: "send", key, text: combined, attachments: [] });
      configRef.current.sendQueued(key, combined, dispatch);
    }, 400);
  }, []);

  useEffect(() => {
    let cancelled = false;
    let unEvent: (() => void) | undefined;
    let unConfirm: (() => void) | undefined;
    let unFile: (() => void) | undefined;
    let unCompaction: (() => void) | undefined;
    let unProposal: (() => void) | undefined;
    const onEvent = (p: AgentEventPayload) => {
      const key = configRef.current.routeAgentEvent(p);
      if (key == null) return;
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
          const run = stateRef.current[key]?.run ?? null;
          dispatch({
            type: "done",
            key,
            answer: p.event.answer,
            reason: p.event.reason,
            reasoning: p.event.reasoning,
          });
          configRef.current.onRunEnd?.(key, run, dispatch);
          flushQueue(key);
          break;
        }
        case "error": {
          const run = stateRef.current[key]?.run ?? null;
          dispatch({ type: "runError", key, message: p.event.message, code: p.event.code });
          configRef.current.onRunEnd?.(key, run, dispatch);
          flushQueue(key);
          break;
        }
        case "confirmationRequested":
          break; // the id-bearing copy arrives as "agent-confirmation"
      }
    };
    void ipc.onAgentEvent(onEvent).then((u) => (cancelled ? u() : (unEvent = u)));
    void ipc
      .onAgentConfirmation((p) => {
        const key = configRef.current.routeConfirmation(p);
        if (key == null) return;
        dispatch({ type: "confirm", key, payload: p });
      })
      .then((u) => (cancelled ? u() : (unConfirm = u)));
    void ipc
      .onAgentFile((p) => {
        const key = configRef.current.routeAgentFile(p);
        if (key == null) return;
        dispatch({ type: "agentFile", key, payload: p });
      })
      .then((u) => (cancelled ? u() : (unFile = u)));
    void ipc
      .onAgentCompaction((p) => {
        const key = configRef.current.routeCompaction(p);
        if (key == null) return;
        dispatch({ type: "compaction", key, summary: p.summary });
      })
      .then((u) => (cancelled ? u() : (unCompaction = u)));
    void ipc
      .onSkillProposal((p) => {
        const key = configRef.current.routeSkillProposal(p);
        if (key == null) return;
        dispatch({ type: "skillProposal", key, messageId: p.messageId, text: p.text });
      })
      .then((u) => (cancelled ? u() : (unProposal = u)));
    return () => {
      cancelled = true;
      unEvent?.();
      unConfirm?.();
      unFile?.();
      unCompaction?.();
      unProposal?.();
    };
    // The subscriptions are re-established only when the view's own stable
    // deps change (actions / loadSessions / loadSchedules), exactly as before;
    // routing and side effects are read through configRef, so they stay fresh.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, subscribeDeps);

  return { state, stateRef, dispatch };
}

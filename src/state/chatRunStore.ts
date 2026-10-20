import { useSyncExternalStore } from "react";
import * as ipc from "../api/ipc";
import { toMessage } from "../shared/errors";
import {
  chatReducer,
  combineQueued,
  summarizeArgs,
  type ChatAction,
  type ChatState,
  type Slice,
} from "../components/chat/chatShared";
import type {
  AgentCompactionEvent,
  AgentEventPayload,
  AgentFileEvent,
  ConfirmationRequestPayload,
  MessageAttachmentDto,
  SkillProposalEvent,
} from "../api/types";

// App-level chat-run store: per-chat run state and the five backend
// subscriptions live here, not in the chat components, so a run keeps
// streaming when its view unmounts (tab/project switch) and several chats
// run in parallel. State is keyed by a namespaced chat key so project ids and
// session ids never collide: "p:<projectId>", "s:<sessionId>", and the
// per-working-dir notes chat singleton under the fixed "notes" key.

export const projectKey = (id: number) => `p:${id}`;
export const sessionKey = (id: number) => `s:${id}`;
export const NOTES_KEY = "notes";

let slices: ChatState = {};
// chatId -> chat key, learned from agent events (which carry both) and from
// the views' history loads / send returns; agent-file and compaction events
// carry only a chatId, so they route back through this map.
const chatIdToKey = new Map<number, string>();
// The chat the user is currently viewing: a run that ends for another chat
// marks that slice "ready" (a dot in its list) until the chat is opened.
let openKey: string | null = null;

const listeners = new Set<() => void>();
const runEndListeners = new Set<(key: string, wasLocal: boolean) => void>();
const sentListeners = new Set<(key: string) => void>();

function emit() {
  for (const l of listeners) l();
}

export function getSnapshot(): ChatState {
  return slices;
}

export function getSlice(key: string): Slice | undefined {
  return slices[key];
}

export function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

export function dispatch(action: ChatAction) {
  slices = chatReducer(slices, action);
  emit();
}

export function rememberChatId(key: string, chatId: number) {
  chatIdToKey.set(chatId, key);
}

export function openChat(key: string) {
  openKey = key;
  dispatch({ type: "seen", key });
}

export function closeChat(key: string) {
  if (openKey === key) openKey = null;
}

export function onRunEnd(cb: (key: string, wasLocal: boolean) => void): () => void {
  runEndListeners.add(cb);
  return () => {
    runEndListeners.delete(cb);
  };
}

export function onSent(cb: (key: string) => void): () => void {
  sentListeners.add(cb);
  return () => {
    sentListeners.delete(cb);
  };
}

// Start a run for a chat: open the live run slice, then issue the per-kind
// send command and remember the returned chat id. The view does its own
// pre-steps (attachment upload, session auto-create) before calling this.
export function send(key: string, text: string, attachments: MessageAttachmentDto[]) {
  dispatch({ type: "send", key, text, attachments });
  const kind = key[0];
  const id = Number(key.slice(2));
  let p: Promise<unknown>;
  if (kind === "p") {
    p = ipc.sendProjectMessage(id, text, attachments).then((chat) => rememberChatId(key, chat));
  } else if (kind === "s") {
    p = ipc.sendSessionMessage(id, text, attachments).then((chat) => rememberChatId(key, chat));
  } else {
    p = ipc.sendNotesMessage(text, attachments);
  }
  p.then(() => {
    for (const l of sentListeners) l(key);
  }).catch((e) => dispatch({ type: "sendFailed", key, message: toMessage(e) }));
}

export function requestStop(key: string) {
  dispatch({ type: "stopping", key });
  const kind = key[0];
  const id = Number(key.slice(2));
  const p =
    kind === "p"
      ? ipc.cancelRun(id)
      : kind === "s"
        ? ipc.cancelSessionRun(id)
        : ipc.cancelNotesRun();
  p.then((ok) => {
    if (!ok) dispatch({ type: "stopCleared", key });
  }).catch(() => dispatch({ type: "stopCleared", key }));
}

export function flushHeldQueue(key: string) {
  if (slices[key]?.run) return;
  flushQueue(key);
}

// A run finished: notify mounted views (history reload / list refresh), mark a
// chat the user is not looking at as "ready", then flush its queued messages —
// except after a cancellation, which holds the queue for an explicit send.
function finishRun(key: string, wasLocal: boolean, reason?: string) {
  for (const l of runEndListeners) l(key, wasLocal);
  if (key !== openKey) dispatch({ type: "ready", key });
  if (reason !== "cancelled") flushQueue(key);
}

// Flush the queued messages as one combined user turn once the run is idle.
// The short delay lets the finished run persist its answer first; if another
// run started meanwhile the text is re-queued for its end.
function flushQueue(key: string) {
  const queued = slices[key]?.queue ?? [];
  if (queued.length === 0) return;
  dispatch({ type: "clearQueue", key });
  const combined = combineQueued(queued);
  setTimeout(() => {
    if (slices[key]?.run) {
      dispatch({ type: "enqueue", key, text: combined });
      return;
    }
    send(key, combined, []);
  }, 400);
}

function routeAgentEvent(p: AgentEventPayload): string | null {
  if (p.projectId != null) return projectKey(p.projectId);
  if (p.sessionId != null) return sessionKey(p.sessionId);
  if (p.notes === true) return NOTES_KEY;
  return null;
}

function onAgentEvent(p: AgentEventPayload) {
  const key = routeAgentEvent(p);
  if (key == null) return;
  chatIdToKey.set(p.chatId, key);
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
      const wasLocal = slices[key]?.run?.local ?? false;
      dispatch({
        type: "done",
        key,
        answer: p.event.answer,
        reason: p.event.reason,
        reasoning: p.event.reasoning,
      });
      finishRun(key, wasLocal, p.event.reason);
      break;
    }
    case "error": {
      const wasLocal = slices[key]?.run?.local ?? false;
      dispatch({ type: "runError", key, message: p.event.message, code: p.event.code });
      finishRun(key, wasLocal);
      break;
    }
    case "confirmationRequested":
      break; // the id-bearing copy arrives as "agent-confirmation"
  }
}

function onConfirmation(p: ConfirmationRequestPayload) {
  let key: string | null = null;
  if (p.projectId != null) key = projectKey(p.projectId);
  else if (p.sessionId != null) key = sessionKey(p.sessionId);
  else if (p.notes === true) key = NOTES_KEY;
  if (key != null) dispatch({ type: "confirm", key, payload: p });
}

function onAgentFile(p: AgentFileEvent) {
  const key = chatIdToKey.get(p.chatId);
  if (key != null) dispatch({ type: "agentFile", key, payload: p });
}

function onCompaction(p: AgentCompactionEvent) {
  const key = chatIdToKey.get(p.chatId);
  if (key != null) dispatch({ type: "compaction", key, summary: p.summary });
}

function onSkillProposal(p: SkillProposalEvent) {
  let key: string | null = null;
  if (p.projectId != null) key = projectKey(p.projectId);
  else if (p.sessionId != null) key = sessionKey(p.sessionId);
  else if (p.notes === true) key = NOTES_KEY;
  else key = chatIdToKey.get(p.chatId) ?? null;
  if (key != null) dispatch({ type: "skillProposal", key, messageId: p.messageId, text: p.text });
}

// The five subscriptions are established once for the whole app lifetime and
// never torn down, so a run keeps streaming regardless of which view is
// mounted. Idempotent; called from the chat views on mount and from App boot.
let subscribed = false;
export function ensureSubscribed() {
  if (subscribed) return;
  subscribed = true;
  void ipc.onAgentEvent(onAgentEvent).catch(() => {});
  void ipc.onAgentConfirmation(onConfirmation).catch(() => {});
  void ipc.onAgentFile(onAgentFile).catch(() => {});
  void ipc.onAgentCompaction(onCompaction).catch(() => {});
  void ipc.onSkillProposal(onSkillProposal).catch(() => {});
}

// ---------- React bindings ----------

export function useChatSlice(key: string): Slice | null {
  return useSyncExternalStore(
    subscribe,
    () => slices[key] ?? null,
    () => null,
  );
}

export function useChatRunning(key: string): boolean {
  return useSyncExternalStore(
    subscribe,
    () => slices[key]?.run != null,
    () => false,
  );
}

export function useChatReady(key: string): boolean {
  return useSyncExternalStore(
    subscribe,
    () => slices[key]?.ready === true,
    () => false,
  );
}

// Whole snapshot for list views that show a status dot per chat.
export function useChatSnapshot(): ChatState {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

// Test-only: drop all per-chat state and routing between cases.
export function resetChatRunStore() {
  slices = {};
  chatIdToKey.clear();
  openKey = null;
  runEndListeners.clear();
  sentListeners.clear();
  emit();
}

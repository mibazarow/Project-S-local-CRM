// Harness sessions IPC (SPEC 5.1) and the session-list/chat-open events.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ChatOpenEvent, MessageAttachmentDto, MessageDto, SessionDto } from "../types";

// Harness sessions (SPEC 5.1): independent chats sharing only global memory.
// `greeting` seeds the chat with one persisted static assistant message (no
// LLM call) — used by the schedules panel «+» to open a schedule-creation chat.
export const harnessCreateSession = (title?: string, greeting?: string) =>
  invoke<SessionDto>("harness_create_session", {
    title: title ?? null,
    greeting: greeting ?? null,
  });

export const harnessListSessions = () => invoke<SessionDto[]>("harness_list_sessions");

export const harnessRenameSession = (id: number, title: string) =>
  invoke<SessionDto>("harness_rename_session", { id, title });

export const harnessDeleteSession = (id: number) => invoke<void>("harness_delete_session", { id });

// Pin/unpin a Harness chat; returns the fresh session list (pinned first).
export const harnessPinSession = (id: number, pinned: boolean) =>
  invoke<SessionDto[]>("harness_pin_session", { id, pinned });

// Bind the chat to a folder (null clears the binding); returns the fresh list.
export const harnessSetWorkingDir = (id: number, path: string | null) =>
  invoke<SessionDto[]>("harness_set_working_dir", { id, path });

export const harnessSessionHistory = (id: number) =>
  invoke<MessageDto[]>("harness_session_history", { id });

export const sendSessionMessage = (
  sessionId: number,
  text: string,
  attachments?: MessageAttachmentDto[],
) =>
  invoke<number>("send_session_message", {
    sessionId,
    text,
    attachments: attachments && attachments.length ? attachments : null,
  });

export const cancelSessionRun = (sessionId: number) =>
  invoke<boolean>("cancel_session_run", { sessionId });

// Emitted when the session list may have changed: the scheduler after a
// fired tick (a fire touches lastActiveAt and may create a fresh session),
// the session auto-title after the first message, and the rename_chat tool.
export const onSessionsChanged = (handler: () => void) =>
  listen<unknown>("sessions-changed", () => handler());

// Emitted by the open_chat tool: the UI must switch to the chat the agent
// found by meaning (Q.md Harness chat search).
export const onChatOpen = (handler: (payload: ChatOpenEvent) => void) =>
  listen<ChatOpenEvent>("chat-open", (e) => handler(e.payload));

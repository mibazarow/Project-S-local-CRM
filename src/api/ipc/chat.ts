// Project-chat IPC: attachments, send/cancel, message-level UX and the
// agent/confirmation backend events.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AgentCompactionEvent,
  AgentCompactionFailedEvent,
  AgentEventPayload,
  AgentFileEvent,
  AttachmentDto,
  ChatBranchResult,
  ConfirmationRequestPayload,
  MessageAttachmentDto,
  MessageDto,
  SkillProposalEvent,
} from "../types";

// Chat attachments (SPEC 6.1): attach_files copies sources into the project's,
// the harness or the Notes inbox; exactly one of projectId / sessionId / notes
// selects the target. The data-url/open commands resolve project-relative and
// absolute savedPaths alike (absolute also covers harness and notes
// attachments and pending source paths before saving).
export const attachFiles = (
  projectId: number | null,
  sessionId: number | null,
  paths: string[],
  notes?: boolean,
) =>
  invoke<AttachmentDto[]>("attach_files", {
    projectId,
    sessionId,
    notes: notes ?? null,
    paths,
  });

export const attachmentDataUrl = (savedPath: string, projectId?: number, maxBytes?: number) =>
  invoke<string>("attachment_data_url", {
    projectId: projectId ?? null,
    savedPath,
    maxBytes: maxBytes ?? null,
  });

export const attachmentOpen = (savedPath: string, projectId?: number) =>
  invoke<void>("attachment_open", { projectId: projectId ?? null, savedPath });

// Agent-sent chat files (SPEC 6.1): the savedPath of a send_file attachment is
// resolved by the chat's kind, so reads/opens go through the chat/message/index
// instead of a savedPath + projectId pair.
export const chatAttachmentDataUrl = (
  chatId: number,
  messageId: number,
  index: number,
  maxBytes?: number,
) =>
  invoke<string>("chat_attachment_data_url", {
    chatId,
    messageId,
    index,
    maxBytes: maxBytes ?? null,
  });

export const chatAttachmentOpen = (chatId: number, messageId: number, index: number) =>
  invoke<void>("chat_attachment_open", { chatId, messageId, index });

export const sendProjectMessage = (
  projectId: number,
  text: string,
  attachments?: MessageAttachmentDto[],
) =>
  invoke<number>("send_project_message", {
    projectId,
    text,
    attachments: attachments && attachments.length ? attachments : null,
  });

export const resolveConfirmation = (id: number, decision: "allow" | "deny" | "allowAlways") =>
  invoke<void>("resolve_confirmation", { id, decision });

export const projectChatHistory = (projectId: number) =>
  invoke<MessageDto[]>("project_chat_history", { projectId });

export const cancelRun = (projectId: number) => invoke<boolean>("cancel_run", { projectId });

// Message-level chat UX: pin, truncate (regenerate), branch (harness only),
// in-chat LIKE search.
export const chatPin = (chatId: number, messageId: number, pinned: boolean) =>
  invoke<void>("chat_pin", { chatId, messageId, pinned });

export const chatTruncateAfter = (chatId: number, messageId: number) =>
  invoke<number>("chat_truncate_after", { chatId, messageId });

export const chatBranch = (chatId: number, messageId: number) =>
  invoke<ChatBranchResult>("chat_branch", { chatId, messageId });

export const onAgentEvent = (handler: (payload: AgentEventPayload) => void) =>
  listen<AgentEventPayload>("agent-event", (e) => handler(e.payload));

export const onAgentConfirmation = (handler: (payload: ConfirmationRequestPayload) => void) =>
  listen<ConfirmationRequestPayload>("agent-confirmation", (e) => handler(e.payload));

// Emitted by the send_file tool after the agent attached a file to a chat.
export const onAgentFile = (handler: (payload: AgentFileEvent) => void) =>
  listen<AgentFileEvent>("agent-file", (e) => handler(e.payload));

// Emitted when a run's context crossed the compaction threshold and a new
// checkpoint summary was written (the summary rides the event only).
export const onAgentCompaction = (handler: (payload: AgentCompactionEvent) => void) =>
  listen<AgentCompactionEvent>("agent-context-compacted", (e) => handler(e.payload));

// Emitted when the context crossed the threshold but compaction failed or was
// impossible; the message is shown in the chat and the run continues.
export const onAgentCompactionFailed = (
  handler: (payload: AgentCompactionFailedEvent) => void,
) => listen<AgentCompactionFailedEvent>("agent-compaction-failed", (e) => handler(e.payload));

// Emitted by the repeat detector after a run completed the same complex
// task for the second time (SPEC 6.2 Q20 skill proposal).
export const onSkillProposal = (handler: (payload: SkillProposalEvent) => void) =>
  listen<SkillProposalEvent>("agent-skill-proposal", (e) => handler(e.payload));

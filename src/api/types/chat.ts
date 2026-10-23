// chat::MessageDto, attachments and the agent/chat backend event payloads.

// chat::MessageDto
export interface ToolCallDto {
  id: string;
  name: string;
  arguments: string;
}

// attachments::AttachmentDto — one saved copy returned by attach_files.
// savedPath is project-relative for project chats, absolute for harness ones.
export interface AttachmentDto {
  name: string;
  savedPath: string;
  kind: "image" | "document" | "other";
  size: number;
  isImage: boolean;
  isText: boolean;
}

// attachments::AttachmentRef — send_* input and MessageDto.attachments rows.
export interface MessageAttachmentDto {
  name: string;
  savedPath: string;
  kind: "image" | "document" | "other";
  isImage: boolean;
}

export interface MessageDto {
  id: number;
  chatId: number;
  role: "system" | "user" | "assistant" | "tool";
  content: string | null;
  // Thinking trace of an assistant turn (null when thinking is off).
  reasoning: string | null;
  toolCalls: ToolCallDto[];
  toolCallId: string | null;
  name: string | null;
  createdAt: string;
  position: number;
  attachments: MessageAttachmentDto[];
  // Pinned messages show in the compact bar above the transcript.
  pinned: boolean;
}

// chat::ChatBranchDto — result of chat_branch (harness chats only).
export interface ChatBranchResult {
  chatId: number;
  sessionId: number;
}

// chat::ChatSearchHitDto — one LIKE hit inside a chat.
export interface ChatSearchHit {
  messageId: number;
  role: string;
  snippet: string;
  createdAt: string;
}

// agent::loop AgentEvent (serde tag = "type", rename_all = "camelCase").
export type AgentEvent =
  | { type: "textDelta"; text: string }
  | { type: "reasoningDelta"; text: string }
  | { type: "toolCallStarted"; name: string; args: unknown }
  | { type: "toolCallFinished"; name: string; ok: boolean; envelope: unknown }
  | { type: "confirmationRequested"; tool: string; args: unknown; dangerReason: string | null }
  | { type: "done"; answer: string; reason: string; reasoning: string | null }
  | { type: "error"; code: string; message: string };

// agent::send_file — payload of the "agent-file" event emitted when the agent
// attaches a file to the chat with send_file. savedPath rules match
// MessageAttachmentDto (project-/Notes-vault-relative, absolute for harness).
export interface AgentFileEvent {
  chatId: number;
  messageId: number;
  name: string;
  path: string;
  kind: "image" | "document" | "other";
  isImage: boolean;
  caption: string;
}

// chat::run_agent_inner — payload of the "agent-context-compacted" event
// emitted when a run's build_context folds old turns into a new checkpoint.
// summary is the new checkpoint text shown as a collapsible status item; it
// is never persisted as a chat message.
export interface AgentCompactionEvent {
  chatId: number;
  projectId?: number;
  sessionId?: number;
  notes?: boolean;
  upToPosition: number;
  summary: string;
}

// chat::run_agent_inner — payload of the "agent-compaction-failed" event:
// the context crossed the threshold but could not be compacted. The run keeps
// the uncompacted history and the chat shows this explanation.
export interface AgentCompactionFailedEvent {
  chatId: number;
  projectId?: number;
  sessionId?: number;
  notes?: boolean;
  message: string;
}

// skills::proposals — payload of the "agent-skill-proposal" event emitted
// when the repeat detector posts its one-time skill proposal into a chat
// (SPEC 6.2 Q20). The text is already persisted as the assistant message
// (messageId); an open chat appends it live and a later history reload owns
// the same id, so it never renders twice.
export interface SkillProposalEvent {
  chatId: number;
  messageId: number;
  text: string;
  projectId?: number;
  sessionId?: number;
  notes?: boolean;
}

// chat::AgentEventPayload — project chats carry projectId, harness sessions
// carry sessionId, notes-chat runs carry notes: true (chatId only).
export interface AgentEventPayload {
  chatId: number;
  projectId?: number;
  sessionId?: number;
  notes?: boolean;
  event: AgentEvent;
}

// chat::ConfirmationRequestPayload ("agent-confirmation" event), routed by
// the same run key: projectId for project chats, sessionId for sessions,
// notes: true for notes-chat runs.
export interface ConfirmationRequestPayload {
  id: number;
  projectId?: number;
  sessionId?: number;
  notes?: boolean;
  tool: string;
  args: unknown;
  dangerReason: string | null;
}

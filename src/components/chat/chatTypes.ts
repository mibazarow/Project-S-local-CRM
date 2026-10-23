import type {
  AgentFileEvent,
  ConfirmationRequestPayload,
  MessageAttachmentDto,
} from "../../api/types";

// Transcript model shared by the project chat and the harness session chat:
// one reducer keyed by a numeric id (project id or session id), the same
// streaming/steps/confirmation flow for both (SPEC 6: one loop, one stream).

// Backend AppError code for the provider-silence watchdog (SPEC 6.0.1, Q23):
// the only run error the UI offers «Повторить» for.
export const LLM_TIMEOUT_CODE = "llm-timeout";

export type StepStatus = "run" | "ok" | "err";

export interface Step {
  key: number;
  name: string;
  summary: string;
  status: StepStatus;
}

// A context-compaction status item: the checkpoint summary arrives on the
// "agent-context-compacted" event (never as a chat message) and renders as a
// collapsed «Лимит контекста — делаю резюме» block inside the agent bubble.
export interface Compaction {
  key: number;
  summary: string;
  failed?: boolean;
}

// A rendered attachment. Agent-sent files (send_file) additionally carry the
// id of the message that owns them plus the 0-based index inside that message:
// their savedPath is resolved by the chat's kind, so reads/opens go through
// chatAttachmentDataUrl / chatAttachmentOpen instead of savedPath + projectId.
export interface ChatAttachment extends MessageAttachmentDto {
  messageId?: number;
  index?: number;
}

export interface ChatItem {
  key: number;
  role: "user" | "agent";
  text: string;
  // Thinking trace of an agent turn (empty when thinking is off): shown in a
  // collapsed «Размышления» block above the answer.
  reasoning: string;
  // Saved attachment copies shown on the message (SPEC 6.1): user turns carry
  // files the user attached, agent turns carry files the agent sent.
  attachments: ChatAttachment[];
  // Ids of the DB messages this item was built from; live agent-file items
  // carry the send_file message id so a later history reload can dedupe.
  messageIds: number[];
  // Pinned messages show in the compact bar above the transcript.
  pinned: boolean;
  // Live agent-file item (send_file): one DB message rendered as its own
  // bubble, so it has no counterpart group in a history reload — the id
  // mapping for pin/regenerate skips these.
  synthetic?: boolean;
  steps: Step[];
  // Context-compaction status items surfaced during this agent turn.
  compactions: Compaction[];
  note: string | null;
  error: string | null;
  // True when the run failed with the provider-silence timeout (SPEC 6.0.1):
  // the error bubble offers a «Повторить» button instead of the plain hint.
  retryable?: boolean;
}

export interface Run {
  text: string;
  // Thinking trace streamed live for the running turn.
  reasoning: string;
  steps: Step[];
  // Context-compaction status items streamed live for the running turn.
  compactions: Compaction[];
  error: string | null;
  // True when this view started the run itself ("send"). A run that only
  // arrives as events (a fired schedule) is remote: it is finalized from
  // the persisted transcript, not from the stream.
  local: boolean;
}

export interface Slice {
  loaded: boolean;
  items: ChatItem[];
  run: Run | null;
  confirmations: ConfirmationRequestPayload[];
  sendError: string | null;
  // Messages typed while a run is live: held per chat and flushed as one
  // combined user turn when the run finishes normally; a cancelled run keeps
  // the queue held until the user presses «Отправить оставшееся».
  queue: string[];
  stopping: boolean;
  // A background run finished while this chat was not the open one: the list
  // shows a "ready" dot until the user opens the chat (cleared by "seen").
  ready: boolean;
}

export type ChatState = Record<string, Slice>;

export type ChatAction =
  | { type: "loaded"; key: string; items: ChatItem[] }
  | { type: "send"; key: string; text: string; attachments: MessageAttachmentDto[] }
  | { type: "sendFailed"; key: string; message: string }
  | { type: "delta"; key: string; text: string }
  | { type: "reasoningDelta"; key: string; text: string }
  | { type: "stepStart"; key: string; name: string; summary: string }
  | { type: "stepFinish"; key: string; ok: boolean }
  | { type: "runError"; key: string; message: string; code?: string }
  | { type: "done"; key: string; answer: string; reason: string; reasoning: string | null }
  | { type: "confirm"; key: string; payload: ConfirmationRequestPayload }
  | { type: "confirmResolved"; key: string; id: number }
  | { type: "agentFile"; key: string; payload: AgentFileEvent }
  | { type: "skillProposal"; key: string; messageId: number; text: string }
  | { type: "compaction"; key: string; summary: string; failed?: boolean }
  | { type: "enqueue"; key: string; text: string }
  | { type: "clearQueue"; key: string }
  | { type: "removeFromQueue"; key: string; index: number }
  | { type: "stopping"; key: string }
  | { type: "stopCleared"; key: string }
  | { type: "pin"; key: string; itemKey: number; pinned: boolean }
  | { type: "regenerate"; key: string; itemKey: number }
  | { type: "ready"; key: string }
  | { type: "seen"; key: string };

// Optional per-message actions (pin / regenerate / branch) appear on hover,
// a compact bar keeps pinned messages reachable.
export interface TranscriptActions {
  onPin?: (item: ChatItem, pinned: boolean) => void;
  onRegenerate?: (item: ChatItem) => void;
  onBranch?: (item: ChatItem) => void;
  // Retry the failed request behind a provider-silence timeout (SPEC 6.0.1):
  // re-runs the same user turn without duplicating the failed bubble.
  onRetry?: (item: ChatItem) => void;
  // Cross-entity quick actions: turn a message into a Notes/ note or a task.
  onToNote?: (item: ChatItem) => void;
  onToTask?: (item: ChatItem) => void;
}

// One sequence shared by the reducer and the history mapper, so item/step keys
// stay unique across a transcript however they were produced.
let nextKey = 1;

export function nextItemKey(): number {
  return nextKey++;
}

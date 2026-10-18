import { useEffect, useState, type ReactNode, type RefObject } from "react";
import * as ipc from "../../api/ipc";
import { MarkdownMessage } from "./MarkdownMessage";
import type {
  AgentFileEvent,
  ConfirmationRequestPayload,
  MessageAttachmentDto,
  MessageDto,
} from "../../api/types";

// Vision support for the composer warning: cached/override status read once per
// chat mount and refreshed whenever llm.json changes. Returns the effective
// `supported` flag (null = unknown / never probed).
export function useVisionSupport(): boolean | null {
  const [supported, setSupported] = useState<boolean | null>(null);
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    const load = () =>
      void ipc
        .getVisionStatus()
        .then((s) => !cancelled && setSupported(s.supported))
        .catch(() => {});
    load();
    void ipc
      .onConfigChanged(({ which }) => {
        if (which === "llm") load();
      })
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);
  return supported;
}

// Transcript model shared by the project chat and the harness session chat:
// one reducer keyed by a numeric id (project id or session id), the same
// streaming/steps/confirmation flow for both (SPEC 6: one loop, one stream).

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
  // combined user turn when the run finishes (done/error/cancelled).
  queue: string[];
}

export type ChatState = Record<number, Slice>;

export type ChatAction =
  | { type: "loaded"; key: number; items: ChatItem[] }
  | { type: "send"; key: number; text: string; attachments: MessageAttachmentDto[] }
  | { type: "sendFailed"; key: number; message: string }
  | { type: "delta"; key: number; text: string }
  | { type: "reasoningDelta"; key: number; text: string }
  | { type: "stepStart"; key: number; name: string; summary: string }
  | { type: "stepFinish"; key: number; ok: boolean }
  | { type: "runError"; key: number; message: string }
  | { type: "done"; key: number; answer: string; reason: string; reasoning: string | null }
  | { type: "confirm"; key: number; payload: ConfirmationRequestPayload }
  | { type: "confirmResolved"; key: number; id: number }
  | { type: "agentFile"; key: number; payload: AgentFileEvent }
  | { type: "compaction"; key: number; summary: string }
  | { type: "enqueue"; key: number; text: string }
  | { type: "clearQueue"; key: number }
  | { type: "pin"; key: number; itemKey: number; pinned: boolean }
  | { type: "regenerate"; key: number; itemKey: number };

let nextKey = 1;

const emptySlice: Slice = {
  loaded: false,
  items: [],
  run: null,
  confirmations: [],
  sendError: null,
  queue: [],
};

function withSlice(state: ChatState, key: number, fn: (s: Slice) => Slice): ChatState {
  const prev = state[key] ?? emptySlice;
  return { ...state, [key]: fn(prev) };
}

export function chatReducer(state: ChatState, action: ChatAction): ChatState {
  switch (action.type) {
    case "loaded":
      // Never overwrite items while a run is live: the DB copy already
      // contains the persisted user turn and would duplicate live steps.
      return withSlice(state, action.key, (s) =>
        s.run ? { ...s, loaded: true } : { ...s, loaded: true, items: action.items },
      );
    case "send":
      return withSlice(state, action.key, (s) => ({
        ...s,
        loaded: true,
        sendError: null,
        items: [
          ...s.items,
          {
            key: nextKey++,
            role: "user",
            text: action.text,
            reasoning: "",
            attachments: action.attachments,
            messageIds: [],
            pinned: false,
            steps: [],
            compactions: [],
            note: null,
            error: null,
          },
        ],
        run: { text: "", reasoning: "", steps: [], compactions: [], error: null, local: true },
      }));
    case "sendFailed":
      return withSlice(state, action.key, (s) => ({ ...s, run: null, sendError: action.message }));
    case "delta":
    case "reasoningDelta":
      // The first event of a run this view did not start (a fired schedule)
      // opens the run slice so the session streams live either way.
      return withSlice(state, action.key, (s) => {
        const open: Run = { text: "", reasoning: "", steps: [], compactions: [], error: null, local: false };
        if (!s.run) {
          return {
            ...s,
            run:
              action.type === "delta"
                ? { ...open, text: action.text }
                : { ...open, reasoning: action.text },
          };
        }
        return action.type === "delta"
          ? { ...s, run: { ...s.run, text: s.run.text + action.text } }
          : { ...s, run: { ...s.run, reasoning: s.run.reasoning + action.text } };
      });
    case "stepStart":
      return withSlice(state, action.key, (s) => {
        const step: Step = {
          key: nextKey++,
          name: action.name,
          summary: action.summary,
          status: "run",
        };
        return s.run
          ? { ...s, run: { ...s.run, steps: [...s.run.steps, step] } }
          : {
              ...s,
              run: { text: "", reasoning: "", steps: [step], compactions: [], error: null, local: false },
            };
      });
    case "stepFinish":
      // Tool calls run sequentially, so the oldest open step is the match.
      return withSlice(state, action.key, (s) => {
        if (!s.run) return s;
        const steps = [...s.run.steps];
        const i = steps.findIndex((st) => st.status === "run");
        if (i >= 0) steps[i] = { ...steps[i], status: action.ok ? "ok" : "err" };
        return { ...s, run: { ...s.run, steps } };
      });
    case "runError":
      return withSlice(state, action.key, (s) => {
        if (!s.run) return s;
        // Remote run: drop the live slice; the view pulls the persisted
        // transcript (the partial run is already in the DB).
        if (!s.run.local) return { ...s, run: null };
        const item: ChatItem = {
          key: nextKey++,
          role: "agent",
          text: s.run.text.trim(),
          reasoning: s.run.reasoning.trim(),
          attachments: [],
          messageIds: [],
          pinned: false,
          steps: s.run.steps,
          compactions: s.run.compactions,
          note: null,
          error: action.message,
        };
        return { ...s, run: null, items: [...s.items, item] };
      });
    case "done":
      return withSlice(state, action.key, (s) => {
        if (!s.run) return s;
        // Remote run: drop the live slice; the view pulls the persisted
        // transcript, which also carries the schedule's user turn.
        if (!s.run.local) return { ...s, run: null };
        const streamed = s.run.text.trim();
        const answer = action.answer.trim();
        const parts = [streamed];
        if (answer && answer !== streamed) parts.push(answer);
        const note =
          action.reason === "step_limit"
            ? "Лимит шагов достигнут — работа остановлена."
            : action.reason === "cancelled"
              ? "Работа остановлена."
              : null;
        const item: ChatItem = {
          key: nextKey++,
          role: "agent",
          text: parts.filter(Boolean).join("\n\n"),
          // The Done event carries the trace accumulated over the whole run;
          // fall back to what this view streamed live.
          reasoning: (action.reasoning ?? s.run.reasoning).trim(),
          attachments: [],
          messageIds: [],
          pinned: false,
          steps: s.run.steps,
          compactions: s.run.compactions,
          note,
          error: s.run.error,
        };
        return { ...s, run: null, items: [...s.items, item] };
      });
    case "confirm":
      return withSlice(state, action.key, (s) =>
        s.confirmations.some((c) => c.id === action.payload.id)
          ? s
          : { ...s, confirmations: [...s.confirmations, action.payload] },
      );
    case "confirmResolved":
      return withSlice(state, action.key, (s) => ({
        ...s,
        confirmations: s.confirmations.filter((c) => c.id !== action.id),
      }));
    case "agentFile": {
      // A file the agent sent with send_file: append it as its own agent item
      // so an open chat shows it without a history reload. Dedupe by the DB
      // message id: a duplicate event or a later history reload (which owns
      // the same id) never renders the file twice.
      const p = action.payload;
      return withSlice(state, action.key, (s) =>
        s.items.some((it) => it.messageIds.includes(p.messageId))
          ? s
          : {
              ...s,
              items: [
                ...s.items,
                {
                  key: nextKey++,
                  role: "agent",
                  text: p.caption,
                  reasoning: "",
                  attachments: [
                    {
                      name: p.name,
                      savedPath: p.path,
                      kind: p.kind,
                      isImage: p.isImage,
                      messageId: p.messageId,
                      index: 0,
                    },
                  ],
                  messageIds: [p.messageId],
                  pinned: false,
                  synthetic: true,
                  steps: [],
                  compactions: [],
                  note: null,
                  error: null,
                },
              ],
            },
      );
    }
    case "compaction":
      // A new checkpoint summary written by this run's build_context: a
      // collapsed status item inside the agent bubble, never a message.
      return withSlice(state, action.key, (s) => {
        const c: Compaction = { key: nextKey++, summary: action.summary };
        return s.run
          ? { ...s, run: { ...s.run, compactions: [...s.run.compactions, c] } }
          : {
              ...s,
              run: { text: "", reasoning: "", steps: [], compactions: [c], error: null, local: false },
            };
      });
    case "enqueue":
      // Typed while a run is live: held in the per-chat queue, flushed as one
      // combined user turn when the run finishes.
      return withSlice(state, action.key, (s) => ({ ...s, queue: [...s.queue, action.text] }));
    case "clearQueue":
      return withSlice(state, action.key, (s) => ({ ...s, queue: [] }));
    case "pin":
      return withSlice(state, action.key, (s) => ({
        ...s,
        items: s.items.map((it) =>
          it.key === action.itemKey ? { ...it, pinned: action.pinned } : it,
        ),
      }));
    case "regenerate":
      // The view already truncated the DB side; drop everything after the
      // user turn that produced this answer and reopen the live run bubble.
      return withSlice(state, action.key, (s) => {
        const idx = s.items.findIndex((it) => it.key === action.itemKey);
        if (idx < 0) return s;
        let userIdx = -1;
        for (let i = idx - 1; i >= 0; i--) {
          if (s.items[i].role === "user") {
            userIdx = i;
            break;
          }
        }
        if (userIdx < 0) return s;
        return {
          ...s,
          items: s.items.slice(0, userIdx + 1),
          run: { text: "", reasoning: "", steps: [], compactions: [], error: null, local: true },
        };
      });
  }
}

export { toMessage } from "../../shared/errors";

export function summarizeArgs(args: unknown): string {
  let obj = args;
  if (typeof args === "string") {
    try {
      obj = JSON.parse(args);
    } catch {
      return args.slice(0, 120);
    }
  }
  if (obj === null || typeof obj !== "object") return String(obj ?? "");
  const parts = Object.entries(obj as Record<string, unknown>)
    .slice(0, 4)
    .map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`)
    .join(", ");
  return parts.length > 120 ? parts.slice(0, 120) + "…" : parts;
}

function toolOk(content: string | null): boolean {
  if (!content) return true;
  try {
    return (JSON.parse(content) as { ok?: boolean }).ok !== false;
  } catch {
    return true;
  }
}

// Groups the flat transcript (user / assistant+tool_calls / tool results)
// into UI bubbles: each user turn, then one agent turn per assistant stretch.
// Assistant turns may carry agent-sent files (send_file, SPEC 6.1): their
// attachments join the agent bubble tagged with the owning message id and the
// per-message index, so the UI can read/open them through the chat-scoped
// commands. Every item records the DB message ids it was built from.
export function historyToItems(dtos: MessageDto[]): ChatItem[] {
  const items: ChatItem[] = [];
  let agent: ChatItem | null = null;
  const stepById = new Map<string, Step>();
  const attachIndex = new Map<number, number>();
  const flush = () => {
    if (
      agent &&
      (agent.text || agent.reasoning || agent.steps.length || agent.error || agent.attachments.length)
    )
      items.push(agent);
    agent = null;
    stepById.clear();
  };
  for (const m of dtos) {
    if (m.role === "user") {
      flush();
      items.push({
        key: nextKey++,
        role: "user",
        text: m.content ?? "",
        reasoning: "",
        attachments: m.attachments ?? [],
        messageIds: [m.id],
        pinned: m.pinned,
        steps: [],
        compactions: [],
        note: null,
        error: null,
      });
    } else if (m.role === "assistant") {
      if (!agent)
        agent = {
          key: nextKey++,
          role: "agent",
          text: "",
          reasoning: "",
          attachments: [],
          messageIds: [],
          pinned: false,
          steps: [],
          compactions: [],
          note: null,
          error: null,
        };
      agent.messageIds.push(m.id);
      // The last assistant message of the group owns the pin flag.
      agent.pinned = m.pinned;
      if (m.content) agent.text += (agent.text ? "\n\n" : "") + m.content;
      if (m.reasoning) agent.reasoning += (agent.reasoning ? "\n\n" : "") + m.reasoning;
      for (const a of m.attachments ?? []) {
        const index = attachIndex.get(m.id) ?? 0;
        attachIndex.set(m.id, index + 1);
        agent.attachments.push({ ...a, messageId: m.id, index });
      }
      for (const tc of m.toolCalls) {
        const step: Step = { key: nextKey++, name: tc.name, summary: summarizeArgs(tc.arguments), status: "run" };
        agent.steps.push(step);
        if (tc.id) stepById.set(tc.id, step);
      }
    } else if (m.role === "tool") {
      if (agent) agent.messageIds.push(m.id);
      const step = m.toolCallId ? stepById.get(m.toolCallId) : undefined;
      if (step) step.status = toolOk(m.content) ? "ok" : "err";
    }
  }
  flush();
  return items;
}

// ---------- Message-level chat UX helpers

// Pin/regenerate need DB message ids; a live item (a finished run) carries
// none, so the view refetches the history and maps by position: the nth
// non-synthetic item of the same role corresponds to the nth group in the
// fresh transcript (agent-file items own their id and are skipped).
export function freshIdsForItem(fresh: ChatItem[], items: ChatItem[], itemKey: number): number[] {
  const idx = items.findIndex((it) => it.key === itemKey);
  if (idx < 0) return [];
  const item = items[idx];
  if (item.messageIds.length > 0) return item.messageIds;
  const rank = items.slice(0, idx).filter((it) => it.role === item.role && !it.synthetic).length;
  let seen = -1;
  for (const it of fresh) {
    if (it.role !== item.role || it.synthetic) continue;
    seen++;
    if (seen === rank) return it.messageIds;
  }
  return [];
}

// The user turn that precedes the target item, resolved in the fresh
// transcript: its DB id is the truncate boundary for regenerate.
export function freshUserBefore(
  fresh: ChatItem[],
  items: ChatItem[],
  itemKey: number,
): ChatItem | null {
  const idx = items.findIndex((it) => it.key === itemKey);
  if (idx < 0) return null;
  const before = items.slice(0, idx).filter((it) => it.role === "user").length;
  if (before === 0) return null;
  let seen = 0;
  for (const it of fresh) {
    if (it.role !== "user") continue;
    seen++;
    if (seen === before) return it;
  }
  return null;
}

// Message-level UX shared by all three chats. A live item (a finished run)
// carries no DB ids, so each handler refetches the history and maps the item by
// position before touching the transcript; the per-view pieces (which history
// command, the chat id, the send command) are passed in, the fetch-and-map and
// error handling are identical here.

// Resolve the DB message id to pin/branch a rendered item: the last id of the
// group the item maps to in the fresh transcript. Empty when it cannot be found.
export async function resolvePinMessageId(
  historyFn: () => Promise<MessageDto[]>,
  items: ChatItem[],
  itemKey: number,
): Promise<number> {
  const fresh = historyToItems(await historyFn().catch(() => []));
  const ids = freshIdsForItem(fresh, items, itemKey);
  return ids[ids.length - 1];
}

// Resolve the regenerate boundary for an agent answer: the user turn that
// precedes it in the fresh transcript (its first DB id truncates after, its text
// is re-sent). Null when there is no such turn.
export async function resolveRegenerateTurn(
  historyFn: () => Promise<MessageDto[]>,
  items: ChatItem[],
  itemKey: number,
): Promise<{ messageId: number; userText: string } | null> {
  const fresh = historyToItems(await historyFn().catch(() => []));
  const user = freshUserBefore(fresh, items, itemKey);
  const messageId = user?.messageIds[0];
  const userText = user?.text ?? "";
  if (!messageId || !userText) return null;
  return { messageId, userText };
}

// Export saves the Markdown transcript as a local download; failures are silent.
export function runChatExport(chatId: number, download: (r: { markdown: string }) => void): void {
  void ipc
    .chatExport(chatId)
    .then(download)
    .catch(() => {});
}

// Case-insensitive occurrence count of the query across item texts (the
// header shows the total; the transcript filters by it).
export function countMatches(items: ChatItem[], query: string): number {
  const q = query.trim().toLowerCase();
  if (!q) return 0;
  let n = 0;
  for (const it of items) {
    const lower = it.text.toLowerCase();
    let i = 0;
    while ((i = lower.indexOf(q, i)) >= 0) {
      n++;
      i += q.length;
    }
  }
  return n;
}

// Wraps every occurrence of the (already lowercased) query in <mark>.
function highlight(text: string, q: string): ReactNode {
  if (!q) return text;
  const lower = text.toLowerCase();
  const parts: ReactNode[] = [];
  let i = 0;
  let mark = 0;
  while (i < text.length) {
    const j = lower.indexOf(q, i);
    if (j < 0) break;
    if (j > i) parts.push(text.slice(i, j));
    parts.push(<mark key={mark++}>{text.slice(j, j + q.length)}</mark>);
    i = j + q.length;
  }
  parts.push(text.slice(i));
  return parts;
}

// Export saves the Markdown transcript as a local download; the app itself
// never writes to disk from the UI.
export function downloadMarkdown(markdown: string, filename: string) {
  const blob = new Blob([markdown], { type: "text/markdown;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export const PinIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M12 16.5V22" />
    <path d="M8.5 3h7l-1 6.5 2.5 3v1.5H7V12.5l2.5-3-1-6.5z" />
  </svg>
);

export const RegenerateIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M3 12a9 9 0 1 0 3-6.7L3 8" />
    <path d="M3 3v5h5" />
  </svg>
);

export const BranchIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M6 3v12" />
    <circle cx="18" cy="6" r="3" />
    <circle cx="6" cy="18" r="3" />
    <path d="M18 9a9 9 0 0 1-9 9" />
  </svg>
);

export const NoteIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5z" />
    <path d="M14 3v5h5M9 13h6M9 17h4" />
  </svg>
);

export const TaskIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <rect x="4" y="4" width="16" height="16" rx="3" />
    <path d="m8.5 12 2.5 2.5 5-5" />
  </svg>
);

// Export + search controls shared by the headers of all three chats.
export function ChatHeaderTools({
  query,
  onQuery,
  matchCount,
  onExport,
  exportDisabled,
}: {
  query: string;
  onQuery: (q: string) => void;
  matchCount: number;
  onExport: () => void;
  exportDisabled?: boolean;
}) {
  return (
    <span className="chat-head-tools">
      {query.trim() !== "" && <span className="chat-search-count">{matchCount}</span>}
      <input
        className="chat-search"
        type="text"
        placeholder="Поиск в чате…"
        value={query}
        onChange={(e) => onQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            onQuery("");
          }
        }}
      />
      <button
        className="ghost-btn small"
        type="button"
        title="Скачать чат в Markdown"
        onClick={onExport}
        disabled={exportDisabled}
      >
        Экспорт
      </button>
    </span>
  );
}

function StepRow({ step }: { step: Step }) {
  return (
    <div className={`tool-step ${step.status === "err" ? "err" : ""}`}>
      <span className="dot" />
      <span className="nm">{step.name}</span>
      <span className="args">{step.summary}</span>
      <span className="res">{step.status === "run" ? "…" : step.status === "ok" ? "ok" : "ош."}</span>
    </div>
  );
}

export function StepsList({ steps }: { steps: Step[] }) {
  if (steps.length === 0) return null;
  return (
    <details className="tool-steps">
      <summary>Действия агента ({steps.length})</summary>
      {steps.map((st) => (
        <StepRow key={st.key} step={st} />
      ))}
    </details>
  );
}

// Thinking trace of an agent turn: a collapsed «Размышления» block above the
// answer, Markdown-rendered, live while the run streams.
export function ReasoningBlock({ text }: { text: string }) {
  if (!text.trim()) return null;
  return (
    <details className="reasoning">
      <summary>Размышления</summary>
      <div className="reasoning-body">
        <MarkdownMessage text={text} />
      </div>
    </details>
  );
}

// Context-compaction status item: a collapsed «Лимит контекста — делаю
// резюме» line; expanding it reveals the checkpoint summary. Collapsed by
// default, it never dumps the summary inline.
export function CompactionNote({ compaction }: { compaction: Compaction }) {
  return (
    <details className="compaction">
      <summary>Лимит контекста — делаю резюме</summary>
      <div className="compaction-body">
        <MarkdownMessage text={compaction.summary} />
      </div>
    </details>
  );
}

// Joins the queued messages into one user turn: a single message stays
// verbatim, several are separated by a blank-line "---" divider.
export function combineQueued(queued: string[]): string {
  return queued.length === 1 ? queued[0] : queued.join("\n\n---\n\n");
}

export const AttachFileIcon = () => (
  <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
    <path d="M14 2v6h6" />
  </svg>
);

export const AttachXIcon = () => (
  <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
    <path d="M18 6 6 18M6 6l12 12" />
  </svg>
);

// Reads one attachment's bytes as a data URL, lazily (on mount). Agent-sent
// files (messageId + chatId known) go through the chat-scoped command, which
// resolves the savedPath by the chat's kind; user attachments keep the
// savedPath + projectId path.
function useAttachmentUrl(
  attachment: ChatAttachment,
  projectId: number | null,
  chatId: number | null,
): string | null {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    const request =
      attachment.messageId != null && chatId != null
        ? ipc.chatAttachmentDataUrl(chatId, attachment.messageId, attachment.index ?? 0)
        : ipc.attachmentDataUrl(attachment.savedPath, projectId ?? undefined);
    void request
      .then((dataUrl) => {
        if (!cancelled) setUrl(dataUrl);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [attachment.savedPath, attachment.messageId, attachment.index, projectId, chatId]);
  return url;
}

// One image attachment in a message: the bytes come from the backend as a
// data URL; until they arrive or if reading fails, a plain name chip is shown.
// Agent images are clickable (open with the default handler), user ones stay
// plain thumbnails as before.
function AttachmentImage({
  attachment,
  projectId,
  chatId,
}: {
  attachment: ChatAttachment;
  projectId: number | null;
  chatId: number | null;
}) {
  const url = useAttachmentUrl(attachment, projectId, chatId);
  const clickable = attachment.messageId != null && chatId != null;
  const open = () => {
    if (attachment.messageId == null || chatId == null) return;
    void ipc
      .chatAttachmentOpen(chatId, attachment.messageId, attachment.index ?? 0)
      .catch(() => {});
  };
  if (!url) {
    return (
      <div className="msg-attachment doc">
        <AttachFileIcon />
        <span className="cap">{attachment.name}</span>
      </div>
    );
  }
  if (!clickable) {
    return (
      <div className="msg-attachment">
        <img src={url} alt={attachment.name} />
        <div className="cap">{attachment.name}</div>
      </div>
    );
  }
  return (
    <div
      className="msg-attachment clickable"
      role="button"
      tabIndex={0}
      title="Открыть файл"
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter") open();
      }}
    >
      <img src={url} alt={attachment.name} />
      <div className="cap">{attachment.name}</div>
    </div>
  );
}

// Attachments of a message (user or agent): images as thumbnails, documents as
// a name+icon chip; a click opens the saved file with the default macOS
// handler — through the chat-scoped command for agent-sent files, through the
// savedPath + projectId one for user attachments.
export function MessageAttachments({
  attachments,
  projectId,
  chatId,
}: {
  attachments: ChatAttachment[];
  projectId: number | null;
  chatId: number | null;
}) {
  if (attachments.length === 0) return null;
  const open = (a: ChatAttachment, fallbackIndex: number) => {
    if (a.messageId != null && chatId != null) {
      void ipc.chatAttachmentOpen(chatId, a.messageId, a.index ?? fallbackIndex).catch(() => {});
    } else {
      void ipc.attachmentOpen(a.savedPath, projectId ?? undefined).catch(() => {});
    }
  };
  return (
    <>
      {attachments.map((a, i) =>
        a.isImage ? (
          <AttachmentImage key={i} attachment={a} projectId={projectId} chatId={chatId} />
        ) : (
          <div
            key={i}
            className="msg-attachment doc"
            role="button"
            tabIndex={0}
            title="Открыть файл"
            onClick={() => open(a, i)}
            onKeyDown={(e) => {
              if (e.key === "Enter") open(a, i);
            }}
          >
            <AttachFileIcon />
            <span className="cap">{a.name}</span>
          </div>
        ),
      )}
    </>
  );
}

// Transcript column shared by all three chats (project, harness, notes):
// the persisted items plus the live run bubble, with the caller-provided
// empty state and an optional header block (e.g. a list-load error).
// Optional per-message actions (pin / regenerate / branch) appear on hover,
// a compact bar keeps pinned messages reachable, and `search` filters the
// transcript client-side with <mark> highlights.
export interface TranscriptActions {
  onPin?: (item: ChatItem, pinned: boolean) => void;
  onRegenerate?: (item: ChatItem) => void;
  onBranch?: (item: ChatItem) => void;
  // Cross-entity quick actions: turn a message into a Notes/ note or a task.
  onToNote?: (item: ChatItem) => void;
  onToTask?: (item: ChatItem) => void;
}

export function ChatTranscript({
  slice,
  listRef,
  className,
  empty,
  header,
  projectId,
  chatId,
  actions,
  search,
}: {
  slice: Slice | null;
  listRef: RefObject<HTMLDivElement | null>;
  className?: string;
  empty: ReactNode;
  header?: ReactNode;
  projectId: number | null;
  // Id of the chat this transcript shows (null until known): agent-sent file
  // attachments are read/opened through the chat-scoped commands.
  chatId: number | null;
  actions?: TranscriptActions;
  search?: string;
}) {
  const q = (search ?? "").trim().toLowerCase();
  const items = slice?.items ?? [];
  const visible = q ? items.filter((it) => countMatches([it], q) > 0) : items;
  const pinned = items.filter((it) => it.pinned);
  const scrollToKey = (key: number) => {
    listRef.current
      ?.querySelector(`[data-msg-key="${key}"]`)
      ?.scrollIntoView({ block: "center", behavior: "smooth" });
  };
  return (
    <div className={className ?? "chat-messages"} ref={listRef}>
      {header}
      {pinned.length > 0 && (
        <div
          className="pinned-bar"
          role="button"
          tabIndex={0}
          title="Показать в переписке"
          onClick={() => scrollToKey(pinned[0].key)}
          onKeyDown={(e) => {
            if (e.key === "Enter") scrollToKey(pinned[0].key);
          }}
        >
          <PinIcon />
          <span className="txt">{pinned[0].text.trim() || "(сообщение без текста)"}</span>
          {pinned.length > 1 && <span className="cnt">{pinned.length}</span>}
        </div>
      )}
      {!slice || (items.length === 0 && !slice.run) ? (
        <div className="chat-empty">{empty}</div>
      ) : q && visible.length === 0 ? (
        <div className="chat-empty">Ничего не найдено</div>
      ) : (
        <>
          {visible.map((item) => {
            const isLast = items[items.length - 1]?.key === item.key;
            const canRegenerate =
              !!actions?.onRegenerate && item.role === "agent" && isLast && !slice.run;
            const onPin = actions?.onPin;
            const onRegenerate = canRegenerate ? actions?.onRegenerate : undefined;
            const onBranch = actions?.onBranch;
            const hasText = item.text.trim() !== "";
            const onToNote = hasText ? actions?.onToNote : undefined;
            const onToTask = hasText ? actions?.onToTask : undefined;
            return (
              <div
                key={item.key}
                className={`msg ${item.role}${item.pinned ? " pinned" : ""}`}
                data-msg-key={item.key}
              >
                <span className="who">
                  {item.role === "user" ? "Вы" : "Агент"}
                  {item.pinned && (
                    <span className="pin-mark" title="Закреплено">
                      <PinIcon />
                    </span>
                  )}
                </span>
                {(onPin || onRegenerate || onBranch || onToNote || onToTask) && (
                  <span className="msg-actions">
                    {onPin && (
                      <button
                        className={`mini-btn${item.pinned ? " on" : ""}`}
                        type="button"
                        title={item.pinned ? "Открепить" : "Закрепить"}
                        onClick={() => onPin(item, !item.pinned)}
                      >
                        <PinIcon />
                      </button>
                    )}
                    {onRegenerate && (
                      <button
                        className="mini-btn"
                        type="button"
                        title="Перегенерировать ответ"
                        onClick={() => onRegenerate(item)}
                      >
                        <RegenerateIcon />
                      </button>
                    )}
                    {onBranch && (
                      <button
                        className="mini-btn"
                        type="button"
                        title="Создать ветку сессии"
                        onClick={() => onBranch(item)}
                      >
                        <BranchIcon />
                      </button>
                    )}
                    {onToNote && (
                      <button
                        className="mini-btn"
                        type="button"
                        title="В заметку"
                        onClick={() => onToNote(item)}
                      >
                        <NoteIcon />
                      </button>
                    )}
                    {onToTask && (
                      <button
                        className="mini-btn"
                        type="button"
                        title="В задачу"
                        onClick={() => onToTask(item)}
                      >
                        <TaskIcon />
                      </button>
                    )}
                  </span>
                )}
                <ReasoningBlock text={item.reasoning} />
                {item.text &&
                  (q ? (
                    <div className="bubble">{highlight(item.text, q)}</div>
                  ) : item.role === "agent" ? (
                    <MarkdownMessage text={item.text} />
                  ) : (
                    <div className="bubble">{item.text}</div>
                  ))}
                <MessageAttachments
                  attachments={item.attachments}
                  projectId={projectId}
                  chatId={chatId}
                />
                <StepsList steps={item.steps} />
                {item.compactions.map((c) => (
                  <CompactionNote key={c.key} compaction={c} />
                ))}
                {item.note && <span className="note">{item.note}</span>}
                {item.error && (
                  <div className="error">
                    {item.error}
                    <br />
                    Проверьте Настройки → Модель.
                  </div>
                )}
              </div>
            );
          })}
          {slice.run && (
            <div className="msg agent">
              <span className="who">Агент</span>
              <ReasoningBlock text={slice.run.reasoning} />
              {slice.run.text && <MarkdownMessage text={slice.run.text} />}
              <StepsList steps={slice.run.steps} />
              {slice.run.compactions.map((c) => (
                <CompactionNote key={c.key} compaction={c} />
              ))}
              {!slice.run.text &&
                !slice.run.reasoning &&
                slice.run.steps.length === 0 &&
                slice.run.compactions.length === 0 && (
                  <span className="note">агент думает…</span>
                )}
            </div>
          )}
        </>
      )}
    </div>
  );
}

// Pending-confirmation card shared by all three chats (SPEC 6: one loop,
// one confirmation flow; only the routing key differs per view).
export function ConfirmCard({
  confirmation,
  onResolve,
}: {
  confirmation: ConfirmationRequestPayload;
  onResolve: (decision: "allow" | "deny" | "allowAlways") => void;
}) {
  return (
    <div className="confirm-card">
      <div className="head">
        <span className="lbl">Подтверждение</span>
        <span className="tool">{confirmation.tool}</span>
      </div>
      <div className="args">{summarizeArgs(confirmation.args)}</div>
      {confirmation.dangerReason && <div className="danger">{confirmation.dangerReason}</div>}
      <div className="actions">
        <button className="primary-btn small" type="button" onClick={() => onResolve("allow")}>
          Разрешить
        </button>
        <button className="ghost-btn small" type="button" onClick={() => onResolve("allowAlways")}>
          Всегда разрешать
        </button>
        <button
          className="ghost-btn small danger"
          type="button"
          onClick={() => onResolve("deny")}
        >
          Запретить
        </button>
      </div>
    </div>
  );
}

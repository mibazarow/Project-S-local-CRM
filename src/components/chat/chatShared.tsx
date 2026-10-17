import { useEffect, useState, type ReactNode, type RefObject } from "react";
import * as ipc from "../../api/ipc";
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
  // Saved attachment copies shown on the message (SPEC 6.1): user turns carry
  // files the user attached, agent turns carry files the agent sent.
  attachments: ChatAttachment[];
  // Ids of the DB messages this item was built from; live agent-file items
  // carry the send_file message id so a later history reload can dedupe.
  messageIds: number[];
  steps: Step[];
  note: string | null;
  error: string | null;
}

export interface Run {
  text: string;
  steps: Step[];
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
}

export type ChatState = Record<number, Slice>;

export type ChatAction =
  | { type: "loaded"; key: number; items: ChatItem[] }
  | { type: "send"; key: number; text: string; attachments: MessageAttachmentDto[] }
  | { type: "sendFailed"; key: number; message: string }
  | { type: "delta"; key: number; text: string }
  | { type: "stepStart"; key: number; name: string; summary: string }
  | { type: "stepFinish"; key: number; ok: boolean }
  | { type: "runError"; key: number; message: string }
  | { type: "done"; key: number; answer: string; reason: string }
  | { type: "confirm"; key: number; payload: ConfirmationRequestPayload }
  | { type: "confirmResolved"; key: number; id: number }
  | { type: "agentFile"; key: number; payload: AgentFileEvent };

let nextKey = 1;

const emptySlice: Slice = {
  loaded: false,
  items: [],
  run: null,
  confirmations: [],
  sendError: null,
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
            attachments: action.attachments,
            messageIds: [],
            steps: [],
            note: null,
            error: null,
          },
        ],
        run: { text: "", steps: [], error: null, local: true },
      }));
    case "sendFailed":
      return withSlice(state, action.key, (s) => ({ ...s, run: null, sendError: action.message }));
    case "delta":
      // The first event of a run this view did not start (a fired schedule)
      // opens the run slice so the session streams live either way.
      return withSlice(state, action.key, (s) =>
        s.run
          ? { ...s, run: { ...s.run, text: s.run.text + action.text } }
          : { ...s, run: { text: action.text, steps: [], error: null, local: false } },
      );
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
          : { ...s, run: { text: "", steps: [step], error: null, local: false } };
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
          attachments: [],
          messageIds: [],
          steps: s.run.steps,
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
          attachments: [],
          messageIds: [],
          steps: s.run.steps,
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
                  steps: [],
                  note: null,
                  error: null,
                },
              ],
            },
      );
    }
  }
}

export function toMessage(e: unknown): string {
  if (e && typeof e === "object" && "message" in e) {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string" && m) return m;
  }
  return typeof e === "string" ? e : "Неизвестная ошибка";
}

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
    if (agent && (agent.text || agent.steps.length || agent.error || agent.attachments.length))
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
        attachments: m.attachments ?? [],
        messageIds: [m.id],
        steps: [],
        note: null,
        error: null,
      });
    } else if (m.role === "assistant") {
      if (!agent)
        agent = {
          key: nextKey++,
          role: "agent",
          text: "",
          attachments: [],
          messageIds: [],
          steps: [],
          note: null,
          error: null,
        };
      agent.messageIds.push(m.id);
      if (m.content) agent.text += (agent.text ? "\n\n" : "") + m.content;
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
export function ChatTranscript({
  slice,
  listRef,
  className,
  empty,
  header,
  projectId,
  chatId,
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
}) {
  return (
    <div className={className ?? "chat-messages"} ref={listRef}>
      {header}
      {!slice || (slice.items.length === 0 && !slice.run) ? (
        <div className="chat-empty">{empty}</div>
      ) : (
        <>
          {slice.items.map((item) => (
            <div key={item.key} className={`msg ${item.role}`}>
              <span className="who">{item.role === "user" ? "Вы" : "Агент"}</span>
              {item.text && <div className="bubble">{item.text}</div>}
              <MessageAttachments
                attachments={item.attachments}
                projectId={projectId}
                chatId={chatId}
              />
              <StepsList steps={item.steps} />
              {item.note && <span className="note">{item.note}</span>}
              {item.error && (
                <div className="error">
                  {item.error}
                  <br />
                  Проверьте Настройки → Модель.
                </div>
              )}
            </div>
          ))}
          {slice.run && (
            <div className="msg agent">
              <span className="who">Агент</span>
              {slice.run.text && <div className="bubble">{slice.run.text}</div>}
              <StepsList steps={slice.run.steps} />
              {!slice.run.text && slice.run.steps.length === 0 && (
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

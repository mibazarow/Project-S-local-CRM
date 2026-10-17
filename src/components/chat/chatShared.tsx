import { useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type {
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

export interface ChatItem {
  key: number;
  role: "user" | "agent";
  text: string;
  // Saved attachment copies shown on the user's message (SPEC 6.1).
  attachments: MessageAttachmentDto[];
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
  | { type: "confirmResolved"; key: number; id: number };

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
export function historyToItems(dtos: MessageDto[]): ChatItem[] {
  const items: ChatItem[] = [];
  let agent: ChatItem | null = null;
  const stepById = new Map<string, Step>();
  const flush = () => {
    if (agent && (agent.text || agent.steps.length || agent.error)) items.push(agent);
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
        steps: [],
        note: null,
        error: null,
      });
    } else if (m.role === "assistant") {
      if (!agent) agent = { key: nextKey++, role: "agent", text: "", attachments: [], steps: [], note: null, error: null };
      if (m.content) agent.text += (agent.text ? "\n\n" : "") + m.content;
      for (const tc of m.toolCalls) {
        const step: Step = { key: nextKey++, name: tc.name, summary: summarizeArgs(tc.arguments), status: "run" };
        agent.steps.push(step);
        if (tc.id) stepById.set(tc.id, step);
      }
    } else if (m.role === "tool") {
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

// One image attachment in a message: the bytes come from the backend as a
// data URL (the resolver handles project-relative and absolute savedPaths);
// until they arrive or if reading fails, a plain name chip is shown.
function AttachmentImage({
  attachment,
  projectId,
}: {
  attachment: MessageAttachmentDto;
  projectId: number | null;
}) {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    void ipc
      .attachmentDataUrl(attachment.savedPath, projectId ?? undefined)
      .then((dataUrl) => {
        if (!cancelled) setUrl(dataUrl);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [attachment.savedPath, projectId]);
  if (!url) {
    return (
      <div className="msg-attachment doc">
        <AttachFileIcon />
        <span className="cap">{attachment.name}</span>
      </div>
    );
  }
  return (
    <div className="msg-attachment">
      <img src={url} alt={attachment.name} />
      <div className="cap">{attachment.name}</div>
    </div>
  );
}

// Attachments of a user message: images as thumbnails, documents as a
// name+icon chip that opens the saved file with the default macOS handler.
export function MessageAttachments({
  attachments,
  projectId,
}: {
  attachments: MessageAttachmentDto[];
  projectId: number | null;
}) {
  if (attachments.length === 0) return null;
  const open = (a: MessageAttachmentDto) =>
    void ipc.attachmentOpen(a.savedPath, projectId ?? undefined).catch(() => {});
  return (
    <>
      {attachments.map((a, i) =>
        a.isImage ? (
          <AttachmentImage key={i} attachment={a} projectId={projectId} />
        ) : (
          <div
            key={i}
            className="msg-attachment doc"
            role="button"
            tabIndex={0}
            title="Открыть файл"
            onClick={() => open(a)}
            onKeyDown={(e) => {
              if (e.key === "Enter") open(a);
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

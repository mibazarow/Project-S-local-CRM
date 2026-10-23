import {
  LLM_TIMEOUT_CODE,
  nextItemKey,
  type ChatAction,
  type ChatItem,
  type ChatState,
  type Compaction,
  type Run,
  type Slice,
  type Step,
} from "./chatTypes";

const emptySlice: Slice = {
  loaded: false,
  items: [],
  run: null,
  confirmations: [],
  sendError: null,
  queue: [],
  stopping: false,
  ready: false,
};

function withSlice(state: ChatState, key: string, fn: (s: Slice) => Slice): ChatState {
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
        sendError: null,
        stopping: false,
        items: [
          ...s.items,
          {
            key: nextItemKey(),
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
      return withSlice(state, action.key, (s) => ({
        ...s,
        run: null,
        sendError: action.message,
        stopping: false,
      }));
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
          key: nextItemKey(),
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
        if (!s.run.local) return { ...s, run: null, stopping: false };
        const item: ChatItem = {
          key: nextItemKey(),
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
          retryable: action.code === LLM_TIMEOUT_CODE,
        };
        return { ...s, run: null, stopping: false, items: [...s.items, item] };
      });
    case "done":
      return withSlice(state, action.key, (s) => {
        if (!s.run) return s;
        // Remote run: drop the live slice; the view pulls the persisted
        // transcript, which also carries the schedule's user turn.
        if (!s.run.local) return { ...s, run: null, stopping: false };
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
          key: nextItemKey(),
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
        return { ...s, run: null, stopping: false, items: [...s.items, item] };
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
                  key: nextItemKey(),
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
    case "skillProposal": {
      // The one-time skill proposal from the repeat detector (SPEC 6.2 Q20):
      // append it as its own agent item so an open chat shows it without a
      // history reload. Dedupe by the DB message id: a duplicate event or a
      // later history reload (which owns the same id) never renders it twice.
      const { messageId, text } = action;
      return withSlice(state, action.key, (s) =>
        s.items.some((it) => it.messageIds.includes(messageId))
          ? s
          : {
              ...s,
              items: [
                ...s.items,
                {
                  key: nextItemKey(),
                  role: "agent",
                  text,
                  reasoning: "",
                  attachments: [],
                  messageIds: [messageId],
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
        const c: Compaction = {
          key: nextItemKey(),
          summary: action.summary,
          ...(action.failed ? { failed: true } : {}),
        };
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
    case "stopping":
      return withSlice(state, action.key, (s) => (s.stopping ? s : { ...s, stopping: true }));
    case "stopCleared":
      return withSlice(state, action.key, (s) => (s.stopping ? { ...s, stopping: false } : s));
    case "removeFromQueue":
      // Drop one held message by its position; the rest of the queue stays.
      return withSlice(state, action.key, (s) =>
        action.index < 0 || action.index >= s.queue.length
          ? s
          : { ...s, queue: s.queue.filter((_, i) => i !== action.index) },
      );
    case "ready":
      return withSlice(state, action.key, (s) => (s.ready ? s : { ...s, ready: true }));
    case "seen":
      return withSlice(state, action.key, (s) => (s.ready ? { ...s, ready: false } : s));
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

// Joins the queued messages into one user turn: a single message stays
// verbatim, several are separated by a blank-line "---" divider.
export function combineQueued(queued: string[]): string {
  return queued.length === 1 ? queued[0] : queued.join("\n\n---\n\n");
}

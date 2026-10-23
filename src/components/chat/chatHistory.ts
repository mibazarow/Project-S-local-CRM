import type { MessageDto } from "../../api/types";
import { nextItemKey, type ChatItem, type Step } from "./chatTypes";

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
        key: nextItemKey(),
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
          key: nextItemKey(),
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
        const step: Step = { key: nextItemKey(), name: tc.name, summary: summarizeArgs(tc.arguments), status: "run" };
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

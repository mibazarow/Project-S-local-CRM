import type { MessageDto } from "../../api/types";
import { historyToItems } from "./chatHistory";
import type { ChatItem } from "./chatTypes";

// Message-level chat UX helpers

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

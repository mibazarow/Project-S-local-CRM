// harness::SessionDto and search::ChatHit.

export interface SessionDto {
  id: number;
  title: string | null;
  createdAt: string;
  updatedAt: string;
  lastActiveAt: string;
  // Persistent pin (Harness chats only): pinnedAt is the pin stamp, null when
  // unpinned. Pinned sessions sort to the top of the session list.
  pinned: boolean;
  pinnedAt: string | null;
}

// search::ChatHit — one harness session matched by search_chats; the snippet
// carries FTS5 markers ("<<match>>"), score is the merged RRF relevance.
export interface ChatHit {
  sessionId: number;
  title: string | null;
  lastActive: string;
  score: number;
  messageId: number;
  snippet: string;
}

// "chat-open" — emitted by the open_chat tool: the frontend must switch to the
// chat the agent found by meaning (Q.md Harness chat search).
export interface ChatOpenEvent {
  sessionId: number;
}

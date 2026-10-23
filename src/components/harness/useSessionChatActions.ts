import { useCallback, useMemo, type RefObject } from "react";
import * as ipc from "../../api/ipc";
import { toMessage } from "../../shared/errors";
import { resolvePinMessageId, resolveRegenerateTurn } from "../chat/messageActions";
import type { ChatItem, TranscriptActions } from "../chat/chatTypes";
import { dispatch, getSlice, sessionKey } from "../../state/chatRunStore";

// Message-level UX: live items carry no DB ids, so the handlers refetch the
// history and map the item by position before touching the transcript.
export function useSessionChatActions({
  activeId,
  sessionChatIdsRef,
  rememberChatId,
  requestSession,
  setBranchNote,
}: {
  activeId: number | null;
  sessionChatIdsRef: RefObject<Record<number, number>>;
  rememberChatId: (session: number, chatId: number) => void;
  requestSession: (id: number) => void;
  setBranchNote: (note: string) => void;
}): TranscriptActions {
  const pinItem = useCallback(
    async (item: ChatItem, pinned: boolean) => {
      if (activeId == null) return;
      const chatId = sessionChatIdsRef.current[activeId];
      if (chatId == null) return;
      const messageId = await resolvePinMessageId(
        () => ipc.harnessSessionHistory(activeId),
        getSlice(sessionKey(activeId))?.items ?? [],
        item.key,
      );
      if (!messageId) return;
      try {
        await ipc.chatPin(chatId, messageId, pinned);
        dispatch({ type: "pin", key: sessionKey(activeId), itemKey: item.key, pinned });
      } catch {
        // The pin flag simply stays as it was.
      }
    },
    [activeId, sessionChatIdsRef],
  );

  const regenerateItem = useCallback(
    async (item: ChatItem) => {
      if (activeId == null) return;
      const k = sessionKey(activeId);
      const sliceNow = getSlice(k);
      if (sliceNow?.run != null) return;
      const chatId = sessionChatIdsRef.current[activeId];
      if (chatId == null) return;
      const turn = await resolveRegenerateTurn(
        () => ipc.harnessSessionHistory(activeId),
        sliceNow?.items ?? [],
        item.key,
      );
      if (!turn) return;
      try {
        await ipc.chatTruncateAfter(chatId, turn.messageId);
        dispatch({ type: "regenerate", key: k, itemKey: item.key });
        void ipc
          .sendSessionMessage(activeId, turn.userText)
          .then((chat) => rememberChatId(activeId, chat))
          .catch((e) => dispatch({ type: "sendFailed", key: k, message: toMessage(e) }));
      } catch (e) {
        dispatch({ type: "sendFailed", key: k, message: toMessage(e) });
      }
    },
    [activeId, sessionChatIdsRef, rememberChatId],
  );

  // Branching forks the session at a message: the fresh session opens through
  // the pendingSessionId intent, and chat_branch's "sessions-changed" event
  // refreshes the sidebar list.
  const branchItem = useCallback(
    async (item: ChatItem) => {
      if (activeId == null) return;
      const chatId = sessionChatIdsRef.current[activeId];
      if (chatId == null) return;
      const messageId = await resolvePinMessageId(
        () => ipc.harnessSessionHistory(activeId),
        getSlice(sessionKey(activeId))?.items ?? [],
        item.key,
      );
      if (!messageId) return;
      try {
        const res = await ipc.chatBranch(chatId, messageId);
        rememberChatId(res.sessionId, res.chatId);
        requestSession(res.sessionId);
        setBranchNote("Ветка создана — открыта новая сессия.");
      } catch (e) {
        setBranchNote(toMessage(e));
      }
    },
    [activeId, sessionChatIdsRef, rememberChatId, requestSession, setBranchNote],
  );

  return useMemo(
    () => ({ onPin: pinItem, onRegenerate: regenerateItem, onRetry: regenerateItem, onBranch: branchItem }),
    [pinItem, regenerateItem, branchItem],
  );
}

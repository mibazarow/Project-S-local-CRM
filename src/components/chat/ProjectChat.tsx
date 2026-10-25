import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import {
  dispatch,
  ensureSubscribed,
  flushHeldQueue,
  getSlice,
  openChat,
  closeChat,
  onRunEnd,
  projectKey,
  requestStop,
  rememberChatId,
  send as storeSend,
  useChatSlice,
} from "../../state/chatRunStore";
import type { MessageAttachmentDto } from "../../api/types";
import { toMessage } from "../../shared/errors";
import { errorHint } from "./errorHint";
import { historyToItems } from "./chatHistory";
import { resolvePinMessageId, resolveRegenerateTurn } from "./messageActions";
import type { ChatItem } from "./chatTypes";
import { ChatTranscript } from "./message/ChatTranscript";
import { ConfirmCard } from "./actions/ConfirmCard";
import { AttachFileIcon, AttachXIcon } from "./attachments/MessageAttachments";
import { useChatAttachments } from "./attachments/useChatAttachments";
import { ChatComposer } from "./composer/ChatComposer";
import { useVisionSupport } from "./composer/useVisionSupport";
import TodoPanel from "./TodoPanel";
import { createNote, noteNameFromText } from "../shared/crossActions";
import { toast } from "../shared/toast";

interface Props {
  projectId: number | null;
  projectName: string | null;
}

export default function ProjectChat({ projectId, projectName }: Props) {
  const { state: appState, actions } = useApp();
  const key = projectId != null ? projectKey(projectId) : "";
  // project id -> chat id, learned from history loads and send returns:
  // ChatTranscript needs the chat id for agent attachments, and pin/regenerate
  // address the chat by it. The store keeps its own chatId->key map for event
  // routing; this local mirror only serves the transcript and message actions.
  const [chatIds, setChatIds] = useState<Record<number, number>>({});
  const chatIdsRef = useRef(chatIds);
  chatIdsRef.current = chatIds;
  const remember = useCallback(
    (project: number, chat: number) => {
      rememberChatId(projectKey(project), chat);
      setChatIds((prev) => (prev[project] === chat ? prev : { ...prev, [project]: chat }));
    },
    [],
  );
  const [text, setText] = useState("");
  const listRef = useRef<HTMLDivElement>(null);
  const chatRootRef = useRef<HTMLElement | null>(null);
  const att = useChatAttachments(chatRootRef, projectId != null);
  const visionSupported = useVisionSupport();

  const slice = useChatSlice(key);
  const running = slice?.run != null;
  const confirmation = slice?.confirmations[0] ?? null;

  useEffect(() => {
    ensureSubscribed();
  }, []);

  // Track the open chat so a background run finishing elsewhere shows a "ready"
  // dot instead, and clear this chat's ready flag when the user opens it.
  useEffect(() => {
    if (key === "") return;
    openChat(key);
    return () => closeChat(key);
  }, [key]);

  // A finished run may have touched tasks without a matching event: refresh the
  // kanban defensively while this chat is mounted.
  useEffect(() => onRunEnd(() => actions.notifyTasksChanged()), [actions]);

  useEffect(() => {
    if (projectId == null) return;
    if (getSlice(key)?.loaded) return;
    void ipc
      .projectChatHistory(projectId)
      .then((dtos) => {
        const chat = dtos[0]?.chatId;
        if (chat != null) remember(projectId, chat);
        dispatch({ type: "loaded", key, items: historyToItems(dtos) });
      })
      .catch(() => dispatch({ type: "loaded", key, items: [] }));
  }, [projectId, key, remember]);

  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [slice]);

  const sendText = (value: string) => {
    if (projectId == null || running) return;
    const trimmed = value.trim();
    if (!trimmed) return;
    void (async () => {
      let refs: MessageAttachmentDto[] = [];
      if (att.pending.length > 0) {
        try {
          const saved = await ipc.attachFiles(
            projectId,
            null,
            att.pending.map((a) => a.path),
          );
          refs = saved.map((a) => ({
            name: a.name,
            savedPath: a.savedPath,
            kind: a.kind,
            isImage: a.isImage,
          }));
        } catch (e) {
          dispatch({ type: "sendFailed", key, message: toMessage(e) });
          return;
        }
      }
      setText("");
      att.clear();
      storeSend(key, trimmed, refs);
    })();
  };

  // While a run is live the composer stays enabled: sending queues the text
  // (flushed as one combined turn when the run finishes) instead of starting
  // a second run.
  const send = () => {
    if (projectId == null) return;
    const trimmed = text.trim();
    if (!trimmed) return;
    if (running) {
      dispatch({ type: "enqueue", key, text: trimmed });
      setText("");
      return;
    }
    sendText(text);
  };

  // Cross-entity quick action: deliver a prebuilt question through the normal
  // send flow once this project's chat is idle (a live run defers delivery).
  useEffect(() => {
    const ask = appState.pendingAsk;
    if (!ask || ask.projectId !== projectId || running) return;
    actions.clearPendingAsk();
    sendText(ask.text);
  }, [appState.pendingAsk, projectId, running, actions, sendText]);

  // Cross-entity quick action: the text becomes a Notes/ note (name from its
  // first line) or a task in this project (first line as title, full text as
  // comment). Both record a background link to this chat, best-effort.
  const messageToNote = useCallback(
    (item: ChatItem) => {
      const full = item.text.trim();
      if (!full) return;
      const chatId = projectId != null ? chatIdsRef.current[projectId] : undefined;
      void createNote(noteNameFromText(full), full)
        .then((rel) => {
          if (chatId != null) void ipc.linkCreate("note", rel, "chat", String(chatId)).catch(() => {});
          toast("ok", `Заметка «${rel}» создана`);
        })
        .catch((e) => toast("err", toMessage(e)));
    },
    [projectId],
  );

  const messageToTask = useCallback(
    (item: ChatItem) => {
      if (projectId == null) return;
      const full = item.text.trim();
      if (!full) return;
      const chatId = chatIdsRef.current[projectId];
      const title = (full.split(/\r?\n/)[0] ?? "").trim().slice(0, 120) || full.slice(0, 120);
      void (async () => {
        try {
          const dto = await ipc.taskCreate(projectId, title);
          if (full !== title) await ipc.taskUpdate(dto.id, { comment: full });
          if (chatId != null)
            void ipc.linkCreate("task", String(dto.id), "chat", String(chatId)).catch(() => {});
          toast("ok", `Задача «${title}» создана`);
        } catch (e) {
          toast("err", toMessage(e));
        }
      })();
    },
    [projectId],
  );

  const resolve = (decision: "allow" | "deny" | "allowAlways") => {
    if (!confirmation || projectId == null) return;
    void ipc.resolveConfirmation(confirmation.id, decision).catch(() => {});
    dispatch({ type: "confirmResolved", key, id: confirmation.id });
  };

  const stop = () => {
    if (projectId != null) requestStop(key);
  };

  // Message-level UX: live items carry no DB ids, so both handlers refetch
  // the history and map the item by position before touching the transcript.
  const pinItem = useCallback(
    async (item: ChatItem, pinned: boolean) => {
      if (projectId == null) return;
      const chatId = chatIdsRef.current[projectId];
      if (chatId == null) return;
      const messageId = await resolvePinMessageId(
        () => ipc.projectChatHistory(projectId),
        getSlice(key)?.items ?? [],
        item.key,
      );
      if (!messageId) return;
      try {
        await ipc.chatPin(chatId, messageId, pinned);
        dispatch({ type: "pin", key, itemKey: item.key, pinned });
      } catch {
        // The pin flag simply stays as it was.
      }
    },
    [projectId, key],
  );

  const regenerateItem = useCallback(
    async (item: ChatItem) => {
      if (projectId == null) return;
      const sliceNow = getSlice(key);
      if (sliceNow?.run != null) return;
      const chatId = chatIdsRef.current[projectId];
      if (chatId == null) return;
      const turn = await resolveRegenerateTurn(
        () => ipc.projectChatHistory(projectId),
        sliceNow?.items ?? [],
        item.key,
      );
      if (!turn) return;
      try {
        await ipc.chatTruncateAfter(chatId, turn.messageId);
        dispatch({ type: "regenerate", key, itemKey: item.key });
        void ipc
          .sendProjectMessage(projectId, turn.userText)
          .then((chat) => remember(projectId, chat))
          .catch((e) => dispatch({ type: "sendFailed", key, message: toMessage(e) }));
      } catch (e) {
        dispatch({ type: "sendFailed", key, message: toMessage(e) });
      }
    },
    [projectId, key, remember],
  );

  const transcriptActions = useMemo(
    () => ({
      onPin: pinItem,
      onRegenerate: regenerateItem,
      onRetry: regenerateItem,
      onToNote: messageToNote,
      onToTask: messageToTask,
    }),
    [pinItem, regenerateItem, messageToNote, messageToTask],
  );

  return (
    <aside
      className={att.dragOver ? "chat chat-pattern drag-over" : "chat chat-pattern"}
      ref={chatRootRef}
      data-chat-drop
    >
      <div className="chat-head">
        <span className="chat-title">Чат проекта «{projectName ?? "—"}»</span>
        {running && <span className="chat-sub">агент работает…</span>}
      </div>

      <TodoPanel scopeKind="project" scopeId={projectId} />

      <ChatTranscript
        slice={slice}
        listRef={listRef}
        empty="Начните диалог — агент проекта ответит здесь."
        projectId={projectId}
        chatId={projectId != null ? (chatIds[projectId] ?? null) : null}
        actions={transcriptActions}
        onRemoveQueued={(index) => dispatch({ type: "removeFromQueue", key, index })}
        onFlushQueue={() => flushHeldQueue(key)}
      />

      {att.pending.length > 0 && (
        <div className="attach-bar">
          {att.pending.map((a) => (
            <div key={a.key} className="attach-chip">
              {a.isImage && a.previewUrl ? <img src={a.previewUrl} alt="" /> : <AttachFileIcon />}
              <span className="nm">{a.name}</span>
              <button className="x" type="button" title="Убрать" onClick={() => att.remove(a.key)}>
                <AttachXIcon />
              </button>
            </div>
          ))}
        </div>
      )}

      {att.pending.some((a) => a.isImage) && visionSupported !== true && (
        <div className="vision-warn">
          Модель не поддерживает vision — изображение не будет распознано (сохранится в проект)
        </div>
      )}

      {slice?.sendError && (
        <div className="chat-error">
          {slice.sendError}
          <br />
          {errorHint(slice.sendError)}
        </div>
      )}

      {confirmation && <ConfirmCard confirmation={confirmation} onResolve={resolve} />}

      <ChatComposer
        text={text}
        onTextChange={setText}
        onSend={send}
        onStop={stop}
        onClear={() => {
          setText("");
          att.clear();
        }}
        running={running}
        stopping={slice?.stopping ?? false}
        queueCount={slice?.queue.length ?? 0}
        placeholder="Спроси агента о проекте…"
        disabled={projectId == null}
        attach={{ onPick: () => att.pick().catch(() => {}), disabled: projectId == null }}
      />
    </aside>
  );
}

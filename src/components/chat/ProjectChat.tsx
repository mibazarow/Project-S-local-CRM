import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import type { AgentEventPayload, MessageAttachmentDto } from "../../api/types";
import {
  historyToItems,
  toMessage,
  ChatTranscript,
  ChatHeaderTools,
  ConfirmCard,
  AttachFileIcon,
  AttachXIcon,
  countMatches,
  resolvePinMessageId,
  resolveRegenerateTurn,
  useVisionSupport,
  type ChatItem,
} from "./chatShared";
import { useChatRun } from "./useChatRun";
import { ChatComposer } from "./ChatComposer";
import { useChatAttachments } from "./useChatAttachments";
import TodoPanel from "./TodoPanel";
import { createNote, noteNameFromText } from "../shared/crossActions";
import { toast } from "../shared/toast";

interface Props {
  projectId: number | null;
  projectName: string | null;
}

export default function ProjectChat({ projectId, projectName }: Props) {
  const { state: appState, actions } = useApp();
  // project id -> chat id, learned from history loads and send returns: the
  // "agent-file" event carries only chatId, so the view maps it back to the
  // project key, and ChatTranscript needs the chat id for agent attachments.
  const [chatIds, setChatIds] = useState<Record<number, number>>({});
  const chatIdsRef = useRef(chatIds);
  chatIdsRef.current = chatIds;
  const rememberChatId = useCallback(
    (project: number, chat: number) =>
      setChatIds((prev) => (prev[project] === chat ? prev : { ...prev, [project]: chat })),
    [],
  );
  const [text, setText] = useState("");
  const [chatQuery, setChatQuery] = useState("");
  const listRef = useRef<HTMLDivElement>(null);
  const chatRootRef = useRef<HTMLElement | null>(null);
  const att = useChatAttachments(chatRootRef);
  const visionSupported = useVisionSupport();

  // Project chats key by projectId; agent-file/compaction events carry only a
  // chatId, so they route back through the remembered project -> chat map. A
  // finished run only bumps the kanban defensively (no history reload here).
  const { state, stateRef, dispatch } = useChatRun(
    {
      routeAgentEvent: (p: AgentEventPayload) => (p.projectId == null ? null : p.projectId),
      routeConfirmation: (p) => (p.projectId == null ? null : p.projectId),
      routeAgentFile: (p) => {
        const entry = Object.entries(chatIdsRef.current).find(([, chat]) => chat === p.chatId);
        return entry ? Number(entry[0]) : null;
      },
      routeSkillProposal: (p) => {
        const entry = Object.entries(chatIdsRef.current).find(([, chat]) => chat === p.chatId);
        return entry ? Number(entry[0]) : null;
      },
      routeCompaction: (p) => {
        const entry = Object.entries(chatIdsRef.current).find(([, chat]) => chat === p.chatId);
        return entry ? Number(entry[0]) : null;
      },
      onRunEnd: () => actions.notifyTasksChanged(),
      sendQueued: (key, combined, dispatch) => {
        void ipc
          .sendProjectMessage(key, combined)
          .then((chat) => rememberChatId(key, chat))
          .catch((e) => dispatch({ type: "sendFailed", key, message: toMessage(e) }));
      },
    },
    [actions],
  );

  const slice = projectId != null ? (state[projectId] ?? null) : null;
  const running = slice?.run != null;
  const confirmation = slice?.confirmations[0] ?? null;

  useEffect(() => {
    if (projectId == null) return;
    if (stateRef.current[projectId]?.loaded) return;
    void ipc
      .projectChatHistory(projectId)
      .then((dtos) => {
        const chat = dtos[0]?.chatId;
        if (chat != null) rememberChatId(projectId, chat);
        dispatch({ type: "loaded", key: projectId, items: historyToItems(dtos) });
      })
      .catch(() => dispatch({ type: "loaded", key: projectId, items: [] }));
  }, [projectId]);

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
          dispatch({ type: "sendFailed", key: projectId, message: toMessage(e) });
          return;
        }
      }
      setText("");
      att.clear();
      dispatch({ type: "send", key: projectId, text: trimmed, attachments: refs });
      void ipc
        .sendProjectMessage(projectId, trimmed, refs)
        .then((chat) => rememberChatId(projectId, chat))
        .catch((e) => dispatch({ type: "sendFailed", key: projectId, message: toMessage(e) }));
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
      dispatch({ type: "enqueue", key: projectId, text: trimmed });
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

  // Message quick actions: the text becomes a Notes/ note (name from its first
  // line) or a task in this project (first line as title, full text as comment).
  // Both record a background link to this chat (links slice), best-effort.
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
    dispatch({ type: "confirmResolved", key: projectId, id: confirmation.id });
  };

  const stop = () => {
    if (projectId != null) void ipc.cancelRun(projectId).catch(() => {});
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
        stateRef.current[projectId]?.items ?? [],
        item.key,
      );
      if (!messageId) return;
      try {
        await ipc.chatPin(chatId, messageId, pinned);
        dispatch({ type: "pin", key: projectId, itemKey: item.key, pinned });
      } catch {
        // The pin flag simply stays as it was.
      }
    },
    [projectId, stateRef, dispatch],
  );

  const regenerateItem = useCallback(
    async (item: ChatItem) => {
      if (projectId == null) return;
      const sliceNow = stateRef.current[projectId];
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
        dispatch({ type: "regenerate", key: projectId, itemKey: item.key });
        await ipc
          .sendProjectMessage(projectId, turn.userText)
          .then((chat) => rememberChatId(projectId, chat))
          .catch((e) => dispatch({ type: "sendFailed", key: projectId, message: toMessage(e) }));
      } catch (e) {
        dispatch({ type: "sendFailed", key: projectId, message: toMessage(e) });
      }
    },
    [projectId, stateRef, dispatch, rememberChatId],
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
      className={att.dragOver ? "chat drag-over" : "chat"}
      ref={chatRootRef}
      data-chat-drop
    >
      <div className="chat-head">
        <span className="chat-title">Чат проекта «{projectName ?? "—"}»</span>
        {running && <span className="chat-sub">агент работает…</span>}
        <ChatHeaderTools
          query={chatQuery}
          onQuery={setChatQuery}
          matchCount={countMatches(slice?.items ?? [], chatQuery)}
        />
      </div>

      <TodoPanel scopeKind="project" scopeId={projectId} />

      <ChatTranscript
        slice={slice}
        listRef={listRef}
        empty="Начните диалог — агент проекта ответит здесь."
        projectId={projectId}
        chatId={projectId != null ? (chatIds[projectId] ?? null) : null}
        actions={transcriptActions}
        search={chatQuery}
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
          Проверьте Настройки → Модель.
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
        queueCount={slice?.queue.length ?? 0}
        placeholder="Спроси агента о проекте…"
        disabled={projectId == null}
        attach={{ onPick: () => att.pick().catch(() => {}), disabled: projectId == null }}
      />
    </aside>
  );
}

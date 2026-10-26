import { useCallback, useEffect, useRef, useState, type ReactNode, type RefObject } from "react";
import { PinIcon } from "../actions/icons";
import { AttachXIcon } from "../attachments/MessageAttachments";
import { MessageRow, RevealAnswer, usePrefersReducedMotion } from "./MessageBubble";
import { CompactionNote, ReasoningBlock, StepsList } from "./MessageParts";
import type { Slice, TranscriptActions } from "../chatTypes";
import { PROVIDER_HINT } from "../errorHint";

// A message typed while a run is live: shown in the transcript immediately as a
// muted, semi-transparent user bubble with a «В очереди» caption and a remove
// control, so the text is never invisible before it flushes. Once the run ends
// the queue clears and the same text re-arrives as a normal sent bubble.
function QueuedMessage({
  text,
  index,
  onRemove,
}: {
  text: string;
  index: number;
  onRemove?: (index: number) => void;
}) {
  return (
    <div className="msg user queued" data-queued-index={index}>
      <span className="queued-label">В очереди — отправится после ответа</span>
      <div className="bubble queued-bubble">{text}</div>
      {onRemove && (
        <button
          className="queued-x"
          type="button"
          title="Убрать из очереди"
          onClick={() => onRemove(index)}
        >
          <AttachXIcon />
        </button>
      )}
    </div>
  );
}

// Transcript column shared by all three chats (project, harness, notes):
// the persisted items plus the live run bubble, with the caller-provided
// empty state and an optional header block (e.g. a list-load error).
// Optional per-message actions (pin / regenerate / branch) appear on hover,
// a compact bar keeps pinned messages reachable.
export function ChatTranscript({
  slice,
  listRef,
  className,
  empty,
  header,
  projectId,
  chatId,
  actions,
  onRemoveQueued,
  onFlushQueue,
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
  // Remove one held (queued) message by its position in the queue.
  onRemoveQueued?: (index: number) => void;
  onFlushQueue?: () => void;
}) {
  const items = slice?.items ?? [];
  const pinned = items.filter((it) => it.pinned);

  // Progressive reveal (SPEC 6.0): the live run bubble and the answer it just
  // produced share one continuous reveal. runRevealRef tracks how much of the
  // current answer the run bubble already showed, so the settled bubble seeds
  // from it (a fully streamed answer settles instantly; a one-chunk or
  // non-streamed answer continues the typewriter). runSeq bumps once per run so
  // the run bubble's reveal resets for a new run instead of inheriting the last.
  const reduced = usePrefersReducedMotion();
  const runRevealRef = useRef(0);
  const prevRunRef = useRef(false);
  const runSeqRef = useRef(0);
  const hasRun = slice?.run != null;
  if (hasRun && !prevRunRef.current) {
    runSeqRef.current += 1;
    runRevealRef.current = 0;
  }
  prevRunRef.current = hasRun;
  const setRunReveal = useCallback((n: number) => {
    runRevealRef.current = n;
  }, []);
  const runIsSilent =
    slice?.run != null &&
    !slice.run.text &&
    !slice.run.reasoning &&
    slice.run.steps.length === 0 &&
    slice.run.compactions.length === 0;
  const [slowRun, setSlowRun] = useState(false);
  useEffect(() => {
    setSlowRun(false);
    if (!runIsSilent) return;
    const timer = window.setTimeout(() => setSlowRun(true), 60_000);
    return () => window.clearTimeout(timer);
  }, [runIsSilent]);

  const last = items[items.length - 1];
  const lastIsLiveAnswer =
    !!last &&
    last.role === "agent" &&
    !last.synthetic &&
    last.messageIds.length === 0 &&
    last.error == null &&
    last.note == null;
  const lastKey = last?.key;

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
      {!slice || (items.length === 0 && !slice.run && slice.queue.length === 0) ? (
        <div className="chat-empty">{empty}</div>
      ) : (
        <>
          {items.map((item) => (
            <MessageRow
              key={item.key}
              item={item}
              isLast={items[items.length - 1]?.key === item.key}
              hasRun={slice.run != null}
              projectId={projectId}
              chatId={chatId}
              actions={actions}
              reveal={
                lastIsLiveAnswer && item.key === lastKey
                  ? { seed: runRevealRef.current, enabled: !reduced }
                  : null
              }
            />
          ))}
          {slice.run && (
            <div className="msg agent">
              <span className="who">Агент</span>
              <ReasoningBlock text={slice.run.reasoning} />
              {slice.run.text && (
                <RevealAnswer
                  key={runSeqRef.current}
                  text={slice.run.text}
                  enabled={!reduced}
                  onShown={setRunReveal}
                />
              )}
              <StepsList steps={slice.run.steps} />
              {slice.run.compactions.map((c) => (
                <CompactionNote key={c.key} compaction={c} />
              ))}
              {runIsSilent && (
                <span className="note">агент думает…</span>
              )}
              {slowRun && <div className="note chat-slow">{PROVIDER_HINT}</div>}
            </div>
          )}
          {slice.queue.map((text, i) => (
            <QueuedMessage key={i} text={text} index={i} onRemove={onRemoveQueued} />
          ))}
          {!slice.run && slice.queue.length > 0 && onFlushQueue && (
            <div className="queue-flush">
              <button className="primary-btn small" type="button" onClick={onFlushQueue}>
                Отправить оставшееся
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

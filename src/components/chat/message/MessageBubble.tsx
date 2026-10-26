import { memo, useEffect, useState } from "react";
import { MarkdownMessage } from "../MarkdownMessage";
import { BranchIcon, NoteIcon, PinIcon, RegenerateIcon, TaskIcon } from "../actions/icons";
import { MessageAttachments } from "../attachments/MessageAttachments";
import { PROVIDER_HINT, errorHint } from "../errorHint";
import { CompactionNote, ReasoningBlock, StepsList } from "./MessageParts";
import type { ChatItem, TranscriptActions } from "../chatTypes";

// Progressive answer reveal (SPEC 6.0, Q22). The backend already streams
// content deltas per SSE chunk, but a provider may deliver the answer in one
// large chunk or not stream it at all (it then arrives whole in `done`). The
// reveal turns such a single block into a short typewriter: ~30 chars per
// frame, with the step grown so a long answer still settles within
// TYPEWRITER_MAX_FRAMES. `seed` is how much is already on screen, so the live
// run bubble and the settled answer share one continuous reveal and never
// re-type or duplicate what the stream already showed. `enabled` is false for
// prefers-reduced-motion and for settled stop/error items, which show the full
// text instantly.
const TYPEWRITER_CHARS_PER_FRAME = 30;
const TYPEWRITER_MAX_FRAMES = 60;

export function usePrefersReducedMotion(): boolean {
  const query = "(prefers-reduced-motion: reduce)";
  const [reduced, setReduced] = useState(
    () => typeof window !== "undefined" && !!window.matchMedia?.(query).matches,
  );
  useEffect(() => {
    const mq = typeof window !== "undefined" ? window.matchMedia?.(query) : undefined;
    if (!mq) return;
    const onChange = () => setReduced(mq.matches);
    mq.addEventListener?.("change", onChange);
    return () => mq.removeEventListener?.("change", onChange);
  }, []);
  return reduced;
}

export function RevealAnswer({
  text,
  seed,
  enabled,
  onShown,
}: {
  text: string;
  seed?: number;
  enabled: boolean;
  onShown?: (n: number) => void;
}) {
  // A streamed delta is small, so the live bubble starts fully shown and grows
  // at once (no artificial per-token delay). A single large chunk (or a
  // non-streamed answer seeded at 0) starts empty and is typed frame-by-frame.
  const [n, setN] = useState(() =>
    enabled
      ? Math.min(seed ?? (text.length <= TYPEWRITER_CHARS_PER_FRAME ? text.length : 0), text.length)
      : text.length,
  );
  useEffect(() => {
    if (!enabled) {
      setN((v) => (v < text.length ? text.length : v));
      return;
    }
    if (n >= text.length) return;
    // A streamed delta (small growth) appears immediately; only a large jump is
    // revealed as a typewriter.
    if (text.length - n <= TYPEWRITER_CHARS_PER_FRAME) {
      setN(text.length);
      return;
    }
    const step = Math.max(
      TYPEWRITER_CHARS_PER_FRAME,
      Math.ceil((text.length - n) / TYPEWRITER_MAX_FRAMES),
    );
    const id = requestAnimationFrame(() => setN((v) => Math.min(text.length, v + step)));
    return () => cancelAnimationFrame(id);
  }, [n, text, enabled]);
  useEffect(() => {
    onShown?.(n);
  }, [n, onShown]);
  return <MarkdownMessage text={text.slice(0, n)} />;
}

// One settled transcript row. Memoized so a streaming delta (which only
// changes the slice's run) or an unrelated store bump never re-renders the
// settled rows: the props are the item itself plus primitives and the
// view-stable actions object.
export const MessageRow = memo(function MessageRow({
  item,
  isLast,
  hasRun,
  projectId,
  chatId,
  actions,
  reveal,
}: {
  item: ChatItem;
  isLast: boolean;
  hasRun: boolean;
  projectId: number | null;
  chatId: number | null;
  actions?: TranscriptActions;
  reveal?: { seed: number; enabled: boolean } | null;
}) {
  const canRegenerate =
    !!actions?.onRegenerate && item.role === "agent" && isLast && !hasRun && !item.retryable;
  const canRetry = !!actions?.onRetry && item.retryable && isLast && !hasRun;
  const onPin = actions?.onPin;
  const onRegenerate = canRegenerate ? actions?.onRegenerate : undefined;
  const onBranch = actions?.onBranch;
  const hasText = item.text.trim() !== "";
  const onToNote = hasText ? actions?.onToNote : undefined;
  const onToTask = hasText ? actions?.onToTask : undefined;
  return (
    <div
      className={`msg ${item.role}${item.pinned ? " pinned" : ""}`}
      data-msg-key={item.key}
    >
      <span className="who">
        {item.role === "user" ? "Вы" : "Агент"}
        {item.pinned && (
          <span className="pin-mark" title="Закреплено">
            <PinIcon />
          </span>
        )}
      </span>
      {(onPin || onRegenerate || onBranch || onToNote || onToTask) && (
        <span className="msg-actions">
          {onPin && (
            <button
              className={`mini-btn${item.pinned ? " on" : ""}`}
              type="button"
              title={item.pinned ? "Открепить" : "Закрепить"}
              onClick={() => onPin(item, !item.pinned)}
            >
              <PinIcon />
            </button>
          )}
          {onRegenerate && (
            <button
              className="mini-btn"
              type="button"
              title="Перегенерировать ответ"
              onClick={() => onRegenerate(item)}
            >
              <RegenerateIcon />
            </button>
          )}
          {onBranch && (
            <button
              className="mini-btn"
              type="button"
              title="Создать ветку сессии"
              onClick={() => onBranch(item)}
            >
              <BranchIcon />
            </button>
          )}
          {onToNote && (
            <button
              className="mini-btn"
              type="button"
              title="В заметку"
              onClick={() => onToNote(item)}
            >
              <NoteIcon />
            </button>
          )}
          {onToTask && (
            <button
              className="mini-btn"
              type="button"
              title="В задачу"
              onClick={() => onToTask(item)}
            >
              <TaskIcon />
            </button>
          )}
        </span>
      )}
      <ReasoningBlock text={item.reasoning} />
      {item.text &&
        (item.role === "agent" ? (
          reveal ? (
            <RevealAnswer key={item.key} text={item.text} seed={reveal.seed} enabled={reveal.enabled} />
          ) : (
            <MarkdownMessage text={item.text} />
          )
        ) : (
          <div className="bubble">{item.text}</div>
        ))}
      <MessageAttachments
        attachments={item.attachments}
        projectId={projectId}
        chatId={chatId}
      />
      <StepsList steps={item.steps} />
      {item.compactions.map((c) => (
        <CompactionNote key={c.key} compaction={c} />
      ))}
      {item.note && <span className="note">{item.note}</span>}
      {item.error && (
        <div className={`error${item.retryable ? " timeout" : ""}`}>
          {item.error}
          {item.retryable ? (
            canRetry && (
              <>
                <br />
                {PROVIDER_HINT}
                <br />
                <button
                  className="primary-btn small"
                  type="button"
                  onClick={() => actions?.onRetry?.(item)}
                >
                  Повторить
                </button>
              </>
            )
          ) : (
            <>
              <br />
              {errorHint(item.error)}
            </>
          )}
        </div>
      )}
    </div>
  );
});

import { useLayoutEffect, useRef, type ChangeEvent, type KeyboardEvent } from "react";

// Composer shared by the three chats: the auto-growing textarea (Enter sends,
// Shift+Enter inserts a newline), the queue indicator, the optional attach
// button and the send/stop pair. The markup, classes and texts are identical to
// what each view rendered inline; the per-view differences (placeholder, the
// disabled conditions, whether an attach button exists and what clearing does)
// are passed explicitly through props.

export interface ChatComposerProps {
  text: string;
  onTextChange: (value: string) => void;
  onSend: () => void;
  onStop: () => void;
  onClear: () => void;
  running: boolean;
  stopping?: boolean;
  queueCount: number;
  placeholder: string;
  // Disables the textarea (no chat selected yet).
  disabled?: boolean;
  // Extra disable condition for the send button on top of the empty-text check
  // (the notes chat also disables it until its singleton chat id is known).
  sendDisabled?: boolean;
  // Presence renders the attach button; its own disabled flag is per-view.
  attach?: { onPick: () => void; disabled?: boolean };
}

export function ChatComposer({
  text,
  onTextChange,
  onSend,
  onStop,
  onClear,
  running,
  stopping,
  queueCount,
  placeholder,
  disabled,
  sendDisabled,
  attach,
}: ChatComposerProps) {
  const taRef = useRef<HTMLTextAreaElement>(null);

  // Auto-grow to fit the typed text, capped at ten lines. Runs after each text
  // change (typing or a programmatic clear) but never on mount, matching the
  // manual grow() the views used to call from their handlers.
  const first = useRef(true);
  useLayoutEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    const cs = getComputedStyle(ta);
    const lh = parseFloat(cs.lineHeight) || parseFloat(cs.fontSize) * 1.5;
    ta.style.height = Math.min(ta.scrollHeight, 10 * lh + 16) + "px";
  }, [text]);

  const onChange = (e: ChangeEvent<HTMLTextAreaElement>) => onTextChange(e.target.value);
  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      onSend();
    }
  };

  return (
    <div className="chat-input">
      <textarea
        ref={taRef}
        rows={3}
        placeholder={placeholder}
        value={text}
        disabled={disabled}
        onChange={onChange}
        onKeyDown={onKeyDown}
      />
      <div className="chat-tools">
        {attach && (
          <button
            className="icon-btn attach-btn"
            type="button"
            title="Приложить файл или картинку"
            disabled={attach.disabled}
            onClick={() => void attach.onPick()}
          >
            <svg viewBox="0 0 24 24" width="17" height="17" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"><path d="m21.4 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l9.2-9.19a4 4 0 0 1 5.65 5.66l-9.2 9.19a2 2 0 0 1-2.82-2.83l8.49-8.48" /></svg>
          </button>
        )}
        <span className="chat-hint">Shift+Enter — перенос строки, Enter — отправить</span>
        {queueCount > 0 && (
          <span className="queue-note" title="Уйдёт одним сообщением, когда агент закончит">
            В очереди: {queueCount}
          </span>
        )}
        {running ? (
          <button className="ghost-btn small stop-btn" type="button" onClick={onStop}>
            {stopping ? "Останавливаю…" : "Стоп"}
          </button>
        ) : (
          <button className="icon-btn clear-btn" type="button" title="Очистить" onClick={onClear}>
            <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"><path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6" /></svg>
          </button>
        )}
        <button
          className="icon-btn send-btn"
          type="button"
          title={running ? "Отправить в очередь" : "Отправить"}
          disabled={sendDisabled || !text.trim()}
          onClick={onSend}
        >
          <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="m22 2-7 20-4-9-9-4 20-7z" /></svg>
        </button>
      </div>
    </div>
  );
}

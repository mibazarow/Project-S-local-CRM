import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import {
  joinMarkdownBlocks,
  renderMarkdown,
  splitMarkdownBlocks,
} from "../shared/markdown";
import { toMessage } from "../chat/chatShared";

interface Props {
  entry: FsEntry | null;
  onDirtyChange: (dirty: boolean) => void;
}

// "blocks" = live block editor (markdown default), "source" = raw markdown
// textarea, "edit" = plain-text textarea for non-markdown files.
type Mode = "blocks" | "source" | "edit";

type Format = "h1" | "h2" | "h3" | "bold" | "italic" | "underline" | "strike";

const AUTOSAVE_MS = 800;

const WRAP: Partial<Record<Format, [string, string]>> = {
  bold: ["**", "**"],
  italic: ["*", "*"],
  underline: ["<u>", "</u>"],
  strike: ["~~", "~~"],
};

const HEADING: Partial<Record<Format, string>> = {
  h1: "# ",
  h2: "## ",
  h3: "### ",
};

const TOOLBAR: { fmt: Format; label: string; cls: string; title: string }[] = [
  { fmt: "h1", label: "H1", cls: "tb-h", title: "Заголовок 1" },
  { fmt: "h2", label: "H2", cls: "tb-h", title: "Заголовок 2" },
  { fmt: "h3", label: "H3", cls: "tb-h", title: "Заголовок 3" },
  { fmt: "bold", label: "B", cls: "tb-bold", title: "Полужирный" },
  { fmt: "italic", label: "I", cls: "tb-italic", title: "Курсив" },
  { fmt: "underline", label: "U", cls: "tb-under", title: "Подчёркивание" },
  { fmt: "strike", label: "S", cls: "tb-strike", title: "Зачёркивание" },
];

export default function NoteEditor({ entry, onDirtyChange }: Props) {
  const [mode, setMode] = useState<Mode>("blocks");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [saved, setSaved] = useState("");
  const [savedFlash, setSavedFlash] = useState(false);
  const [active, setActive] = useState<number | null>(null);
  const [draft, setDraft] = useState("");
  const blockTaRef = useRef<HTMLTextAreaElement | null>(null);
  const sourceTaRef = useRef<HTMLTextAreaElement | null>(null);
  const pendingCaret = useRef<number | null>(null);
  const relRef = useRef<string | null>(null);

  useEffect(() => {
    setMode("blocks");
    setError(null);
    setText("");
    setSaved("");
    setSavedFlash(false);
    setActive(null);
    setDraft("");
    relRef.current = entry?.relPath ?? null;
    if (!entry) return;
    let cancelled = false;
    setLoading(true);
    void (async () => {
      try {
        const content = await ipc.notesRead(entry.relPath);
        if (!cancelled) {
          setText(content);
          setSaved(content);
        }
      } catch (e) {
        if (!cancelled) setError(toMessage(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [entry?.relPath]);

  useEffect(() => {
    onDirtyChange(entry != null && text !== saved);
    if (text !== saved) setSavedFlash(false);
  }, [entry, text, saved, onDirtyChange]);

  // Debounced autosave through notes_write; the rel guard keeps a late write
  // from marking a freshly opened note as saved.
  useEffect(() => {
    if (!entry || text === saved) return;
    const rel = entry.relPath;
    const timer = setTimeout(() => {
      void ipc
        .notesWrite(rel, text)
        .then(() => {
          if (relRef.current !== rel) return;
          setSaved(text);
          setSavedFlash(true);
        })
        .catch((e) => setError(toMessage(e)));
    }, AUTOSAVE_MS);
    return () => clearTimeout(timer);
  }, [entry?.relPath, text, saved]);

  const blocks = useMemo(
    () => (entry?.isMarkdown ? splitMarkdownBlocks(text) : []),
    [entry?.isMarkdown, text],
  );

  // Block textarea: grow with content, restore the caret after a toolbar
  // action moved it (pendingCaret).
  useEffect(() => {
    const ta = blockTaRef.current;
    if (!ta || active === null) return;
    ta.style.height = "auto";
    ta.style.height = `${ta.scrollHeight}px`;
    if (pendingCaret.current !== null) {
      const pos = pendingCaret.current;
      pendingCaret.current = null;
      ta.focus();
      ta.setSelectionRange(pos, pos);
    }
  }, [draft, active]);

  const startEdit = (index: number) => {
    setActive(index);
    setDraft(blocks[index] ?? "");
  };

  const appendBlock = () => {
    setActive(blocks.length);
    setDraft("");
  };

  const commitBlock = () => {
    if (active === null) return;
    const next = [...blocks];
    if (active < next.length) {
      if (draft.trim() === "" && next.length > 1) next.splice(active, 1);
      else next[active] = draft;
    } else if (draft.trim() !== "") {
      next.push(draft);
    }
    setActive(null);
    setDraft("");
    const joined = joinMarkdownBlocks(next);
    if (joined !== text) setText(joined);
  };

  const cancelBlock = () => {
    setActive(null);
    setDraft("");
  };

  const onBlockKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      cancelBlock();
    } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      commitBlock();
    }
  };

  // Wrap the textarea selection (or prefix its lines for headings) and put
  // the caret/selection back where the user was.
  const applyToTextarea = (
    ta: HTMLTextAreaElement,
    fmt: Format,
    commit: (value: string) => void,
  ) => {
    const value = ta.value;
    const s = ta.selectionStart;
    const e = ta.selectionEnd;
    let next: string;
    let ns: number;
    let ne: number;
    const head = HEADING[fmt];
    if (head) {
      const lineStart = value.lastIndexOf("\n", s - 1) + 1;
      let lineEnd = value.indexOf("\n", Math.max(e, s));
      if (lineEnd === -1) lineEnd = value.length;
      const lines = value.slice(lineStart, lineEnd).split("\n");
      const allHave = lines.every((l) => l.startsWith(head));
      const nextLines = lines.map((l) =>
        allHave ? l.slice(head.length) : head + l.replace(/^#{1,6}\s+/, ""),
      );
      const delta = nextLines[0].length - lines[0].length;
      next = value.slice(0, lineStart) + nextLines.join("\n") + value.slice(lineEnd);
      ns = Math.max(lineStart, s + delta);
      ne = Math.max(lineStart, e + delta);
    } else {
      const [open, close] = WRAP[fmt]!;
      const sel = value.slice(s, e);
      let inner = sel;
      let wrapped = true;
      if (sel.startsWith(open) && sel.endsWith(close) && sel.length >= open.length + close.length) {
        inner = sel.slice(open.length, sel.length - close.length);
        wrapped = false;
      }
      const inserted = wrapped ? open + inner + close : inner;
      next = value.slice(0, s) + inserted + value.slice(e);
      if (wrapped && sel === "") {
        ns = ne = s + open.length;
      } else {
        ns = s;
        ne = s + inserted.length;
      }
    }
    ta.value = next;
    commit(next);
    ta.focus();
    ta.setSelectionRange(ns, ne);
  };

  const applyFormat = (fmt: Format) => {
    const target =
      active !== null ? blockTaRef.current : mode === "source" ? sourceTaRef.current : null;
    if (target) {
      applyToTextarea(target, fmt, active !== null ? setDraft : setText);
      return;
    }
    // No active block: insert a new block at the end and format it there.
    const head = HEADING[fmt];
    const wrap = WRAP[fmt]!;
    const initial = head ?? wrap[0] + wrap[1];
    pendingCaret.current = head ? initial.length : wrap[0].length;
    setActive(blocks.length);
    setDraft(initial);
  };

  const blockTextarea = (
    <textarea
      ref={blockTaRef}
      className="md-block-textarea"
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={commitBlock}
      onKeyDown={onBlockKeyDown}
      autoFocus
    />
  );

  return (
    <section className="center">
      <div className="center-head">
        <h2>{entry ? entry.relPath : "Блокнот"}</h2>
        {entry && (
          <div className="preview-actions">
            {savedFlash && text === saved && (
              <span className="notes-save-state">Сохранено</span>
            )}
            {text !== saved && <span className="notes-save-state dirty">Не сохранено</span>}
            {entry.isMarkdown ? (
              <button
                className="ghost-btn small"
                onClick={() => setMode(mode === "source" ? "blocks" : "source")}
              >
                {mode === "source" ? "Готово" : "Исходник"}
              </button>
            ) : (
              <button
                className="ghost-btn small"
                onClick={() => setMode(mode === "edit" ? "blocks" : "edit")}
              >
                {mode === "edit" ? "Готово" : "Редактировать"}
              </button>
            )}
          </div>
        )}
      </div>
      {error && <div className="tree-error notes-editor-error">{error}</div>}
      {entry?.isMarkdown && (
        <div className="notes-md-toolbar">
          {TOOLBAR.map((t) => (
            <button
              key={t.fmt}
              type="button"
              className={`ghost-btn ${t.cls}`}
              title={t.title}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => applyFormat(t.fmt)}
            >
              {t.label}
            </button>
          ))}
        </div>
      )}
      {!entry ? (
        <div className="center-body">
          <div className="placeholder">
            <h1>Блокнот</h1>
            <p>Выберите заметку слева или создайте новую</p>
          </div>
        </div>
      ) : loading ? (
        <div className="center-body">Загрузка…</div>
      ) : !entry.isMarkdown ? (
        mode === "edit" ? (
          <div className="notes-editor-body">
            <textarea
              className="preview-textarea"
              value={text}
              onChange={(e) => setText(e.target.value)}
              autoFocus
            />
          </div>
        ) : (
          <div className="notes-editor-body">
            <pre className="preview-body">{text}</pre>
          </div>
        )
      ) : mode === "source" ? (
        <div className="notes-editor-body">
          <textarea
            ref={sourceTaRef}
            className="preview-textarea"
            value={text}
            onChange={(e) => setText(e.target.value)}
            autoFocus
          />
        </div>
      ) : (
        <div className="notes-editor-body notes-blocks">
          {blocks.map((block, i) =>
            active === i ? (
              blockTextarea
            ) : (
              <div
                key={i}
                className="md-block"
                title="Клик — редактировать блок"
                onClick={() => startEdit(i)}
              >
                <div
                  className="preview-body md"
                  dangerouslySetInnerHTML={{ __html: renderMarkdown(block) }}
                />
              </div>
            ),
          )}
          {active === blocks.length && blockTextarea}
          {active === null && (
            <div
              className="md-block md-block-add"
              title="Добавить блок"
              onClick={appendBlock}
            />
          )}
        </div>
      )}
    </section>
  );
}

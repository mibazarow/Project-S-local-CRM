import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import { renderMarkdown, splitMarkdownBlocksWithRanges } from "../shared/markdown";
import MarkdownCodeEditor from "../shared/MarkdownCodeEditor";
import { EditorView } from "@codemirror/view";
import VersionHistory from "../shared/VersionHistory";
import { toMessage } from "../../shared/errors";
import { EntryIcon } from "../files/icons";

interface Props {
  entry: FsEntry | null;
  onDirtyChange: (dirty: boolean) => void;
}

// "blocks" = live block editor (markdown default), "source" = raw markdown
// CodeMirror editor. Non-md files are preview-only: text read-only, images/PDF inline,
// everything else a card with an "open in system" action.
type Mode = "blocks" | "source";

type Format = "h1" | "h2" | "h3" | "bold" | "italic" | "underline" | "strike";

const AUTOSAVE_MS = 800;

// NOTES_SCOPE_ID in index/mod.rs: "index-changed" rows for the Notes/ vault
// reach the editor through the debounced reindex watcher (external edits).
const NOTES_SCOPE_ID = -1;

const MAX_IMAGE_BYTES = 20 * 1024 * 1024;
// Must stay in sync with PDF_MAX_BYTES in src-tauri/src/fs_ops.rs.
const MAX_PDF_BYTES = 25 * 1024 * 1024;

const dataUrlToBytes = (dataUrl: string): Uint8Array<ArrayBuffer> => {
  const bin = atob(dataUrl.slice(dataUrl.indexOf(",") + 1));
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
};

const formatSize = (bytes: number): string => {
  if (bytes < 1024) return `${bytes} Б`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(1)} КБ`;
  return `${(kb / 1024).toFixed(1)} МБ`;
};

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

interface FormatEdit {
  from: number;
  to: number;
  insert: string;
  ns: number;
  ne: number;
}

// Compute the text edit for a toolbar format on [s, e) of `value`: wrap the
// selection or toggle the heading prefix on its lines, plus the caret/
// selection the caller should restore afterwards.
const formatEdit = (value: string, s: number, e: number, fmt: Format): FormatEdit => {
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
    return {
      from: lineStart,
      to: lineEnd,
      insert: nextLines.join("\n"),
      ns: Math.max(lineStart, s + delta),
      ne: Math.max(lineStart, e + delta),
    };
  }
  const [open, close] = WRAP[fmt]!;
  const sel = value.slice(s, e);
  let inner = sel;
  let wrapped = true;
  if (sel.startsWith(open) && sel.endsWith(close) && sel.length >= open.length + close.length) {
    inner = sel.slice(open.length, sel.length - close.length);
    wrapped = false;
  }
  const inserted = wrapped ? open + inner + close : inner;
  if (wrapped && sel === "") {
    const at = s + open.length;
    return { from: s, to: e, insert: inserted, ns: at, ne: at };
  }
  return { from: s, to: e, insert: inserted, ns: s, ne: s + inserted.length };
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
  const isPdf = entry != null && entry.kind === "file" && entry.ext === "pdf";
  const [mode, setMode] = useState<Mode>("blocks");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [saved, setSaved] = useState("");
  const [savedFlash, setSavedFlash] = useState(false);
  const [active, setActive] = useState<number | null>(null);
  const [draft, setDraft] = useState("");
  const [historyAbs, setHistoryAbs] = useState<string | null>(null);
  const [reloadTick, setReloadTick] = useState(0);
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  const [pdfUrl, setPdfUrl] = useState<string | null>(null);
  const [external, setExternal] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const [missing, setMissing] = useState(false);
  const [reloadSeq, setReloadSeq] = useState(0);
  const blockTaRef = useRef<HTMLTextAreaElement | null>(null);
  const sourceViewRef = useRef<EditorView | null>(null);
  const blocksRef = useRef<HTMLDivElement | null>(null);
  const textPreRef = useRef<HTMLPreElement | null>(null);
  const pendingCaret = useRef<number | null>(null);
  const relRef = useRef<string | null>(null);
  const restoreRef = useRef<number | null>(null);
  const savedRef = useRef("");
  const dirtyRef = useRef(false);

  useEffect(() => {
    setMode("blocks");
    setError(null);
    setText("");
    setSaved("");
    setSavedFlash(false);
    setActive(null);
    setDraft("");
    setDataUrl(null);
    setPdfUrl(null);
    setExternal(null);
    setConflict(false);
    setMissing(false);
    relRef.current = entry?.relPath ?? null;
    if (!entry) return;
    let cancelled = false;
    let objectUrl: string | null = null;
    const isPdfEntry = entry.kind === "file" && entry.ext === "pdf";
    if (entry.isMarkdown || entry.isText) {
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
    } else if (entry.isImage) {
      setLoading(true);
      void (async () => {
        try {
          if (entry.size > MAX_IMAGE_BYTES) {
            if (!cancelled)
              setError(
                `Файл слишком большой для предпросмотра (лимит ${MAX_IMAGE_BYTES / (1024 * 1024)} МБ)`,
              );
          } else {
            const url = await ipc.notesReadBinaryDataUrl(entry.relPath, MAX_IMAGE_BYTES);
            if (!cancelled) setDataUrl(url);
          }
        } catch (e) {
          if (!cancelled) setError(toMessage(e));
        } finally {
          if (!cancelled) setLoading(false);
        }
      })();
    } else if (isPdfEntry) {
      setLoading(true);
      void (async () => {
        try {
          if (entry.size > MAX_PDF_BYTES) {
            if (!cancelled)
              setError(
                `Файл слишком большой для предпросмотра (лимит ${MAX_PDF_BYTES / (1024 * 1024)} МБ)`,
              );
          } else {
            const pdfDataUrl = await ipc.notesReadBinaryDataUrl(
              entry.relPath,
              MAX_PDF_BYTES,
            );
            objectUrl = URL.createObjectURL(
              new Blob([dataUrlToBytes(pdfDataUrl)], { type: "application/pdf" }),
            );
            if (cancelled) URL.revokeObjectURL(objectUrl);
            else setPdfUrl(objectUrl);
          }
        } catch (e) {
          if (!cancelled) setError(toMessage(e));
        } finally {
          if (!cancelled) setLoading(false);
        }
      })();
    }
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [entry?.relPath, reloadTick]);

  useEffect(() => {
    onDirtyChange(entry != null && text !== saved);
    if (text !== saved) setSavedFlash(false);
  }, [entry, text, saved, onDirtyChange]);

  savedRef.current = saved;
  // A block being edited (uncommitted draft) counts as an unsaved edit too, so
  // an external change raises the banner instead of dropping the in-progress block.
  dirtyRef.current = entry != null && (text !== saved || active !== null);

  const scroller = (): HTMLElement | null =>
    mode === "source"
      ? null // CodeMirror keeps its own scroll/caret across a value sync
      : entry?.isMarkdown
        ? blocksRef.current
        : textPreRef.current;

  // Replace the shown content with freshly read disk content, keeping the
  // scroll position (the CodeMirror source view syncs its doc in place).
  const applyExternal = useCallback(
    (content: string) => {
      const el = scroller();
      restoreRef.current = el ? el.scrollTop : null;
      setText(content);
      setSaved(content);
      setExternal(null);
      setConflict(false);
      setMissing(false);
      setReloadSeq((s) => s + 1);
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [mode, entry?.isMarkdown],
  );

  useLayoutEffect(() => {
    if (reloadSeq === 0) return;
    const el = scroller();
    const top = restoreRef.current;
    restoreRef.current = null;
    if (el && top !== null) el.scrollTop = top;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reloadSeq, mode, entry?.isMarkdown]);

  // Re-read the open note when the backend reports a change for this path
  // (notes-changed from our own/agent writes, index-changed from the watcher
  // for edits by another program). Content is diffed against the last loaded
  // version, so our own autosaves and no-op events never loop or raise a banner.
  const checkExternal = useCallback(() => {
    const rel = entry?.relPath;
    if (!rel) return;
    void (async () => {
      let disk: string;
      try {
        disk = await ipc.notesRead(rel);
      } catch {
        setMissing(true);
        setConflict(false);
        setExternal(null);
        return;
      }
      setMissing(false);
      if (disk === savedRef.current) return;
      if (dirtyRef.current) {
        setExternal(disk);
        setConflict(true);
      } else {
        applyExternal(disk);
      }
    })();
  }, [entry?.relPath, applyExternal]);

  useEffect(() => {
    const rel = entry?.relPath;
    if (!rel || !(entry.isMarkdown || entry.isText)) return;
    let cancelled = false;
    let unNotes: (() => void) | undefined;
    let unIndex: (() => void) | undefined;
    void ipc
      .onNotesChanged((p) => {
        if (!cancelled && p.relPath === rel) checkExternal();
      })
      .then((u) => (cancelled ? u() : (unNotes = u)));
    void ipc
      .onIndexChanged((p) => {
        if (!cancelled && p.projectId === NOTES_SCOPE_ID) checkExternal();
      })
      .then((u) => (cancelled ? u() : (unIndex = u)));
    return () => {
      cancelled = true;
      unNotes?.();
      unIndex?.();
    };
  }, [entry?.relPath, entry?.isMarkdown, entry?.isText, checkExternal]);

  // Debounced autosave through notes_write; the rel guard keeps a late write
  // from marking a freshly opened note as saved. Paused while an external
  // change is pending a decision, so the draft never clobbers the disk edit.
  useEffect(() => {
    if (!entry || text === saved || conflict) return;
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

  // Version snapshots are keyed by the absolute vault path; notesEnsure returns
  // the Notes/ root, joined with the vault-relative path of the open note.
  const openHistory = async () => {
    if (!entry) return;
    try {
      const dir = await ipc.notesEnsure();
      setHistoryAbs(`${dir}/${entry.relPath}`);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  // notes_write snapshots the pre-restore content itself, so the current text
  // is kept as a new version; reload the note from disk afterwards.
  const restoreVersion = async (content: string) => {
    if (!entry) return;
    await ipc.notesWrite(entry.relPath, content);
    setReloadTick((t) => t + 1);
  };

  // Existing open-with flow (scope "notes"): remembered app or system default.
  const openInSystem = async () => {
    if (!entry) return;
    try {
      await ipc.openWith("notes", entry.relPath);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const blocks = useMemo(
    () => (entry?.isMarkdown ? splitMarkdownBlocksWithRanges(text) : []),
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
    setDraft(blocks[index]?.text ?? "");
  };

  const appendBlock = () => {
    setActive(blocks.length);
    setDraft("");
  };

  // Replace only the edited block's range in the original text, so blank-line
  // structure elsewhere (runs of blanks, leading/trailing blanks) survives.
  // Deleting a block also removes its following blank-line gap (or the
  // preceding one for the last block); appending adds one blank-line gap.
  const commitBlock = () => {
    if (active === null) return;
    let next = text;
    if (active < blocks.length) {
      const r = blocks[active];
      if (draft.trim() === "" && blocks.length > 1) {
        const from = r.from;
        const to = active + 1 < blocks.length ? blocks[active + 1].from : r.to;
        const cutFrom = active + 1 < blocks.length ? from : blocks[active - 1].to;
        next = text.slice(0, cutFrom) + text.slice(to);
      } else {
        next = text.slice(0, r.from) + draft + text.slice(r.to);
      }
    } else if (draft.trim() !== "") {
      const sep =
        text === "" || /\n[ \t]*\n[ \t]*$/.test(text)
          ? ""
          : /\n[ \t]*$/.test(text)
            ? "\n"
            : "\n\n";
      next = text + sep + draft;
    }
    setActive(null);
    setDraft("");
    if (next !== text) setText(next);
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
    const ed = formatEdit(ta.value, ta.selectionStart, ta.selectionEnd, fmt);
    const next = ta.value.slice(0, ed.from) + ed.insert + ta.value.slice(ed.to);
    ta.value = next;
    commit(next);
    ta.focus();
    ta.setSelectionRange(ed.ns, ed.ne);
  };

  // Same format applied through the CodeMirror source view; the update
  // listener pushes the new doc into `text`.
  const applyToView = (view: EditorView, fmt: Format) => {
    const ed = formatEdit(
      view.state.doc.toString(),
      view.state.selection.main.from,
      view.state.selection.main.to,
      fmt,
    );
    view.dispatch({
      changes: { from: ed.from, to: ed.to, insert: ed.insert },
      selection: { anchor: ed.ns, head: ed.ne },
    });
    view.focus();
  };

  const applyFormat = (fmt: Format) => {
    if (active !== null && blockTaRef.current) {
      applyToTextarea(blockTaRef.current, fmt, setDraft);
      return;
    }
    if (mode === "source" && sourceViewRef.current) {
      applyToView(sourceViewRef.current, fmt);
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

  const textual = entry != null && (entry.isMarkdown || entry.isText);

  return (
    <section className="center">
      <div className="center-head">
        <h2>{entry ? entry.relPath : "Заметки и файлы"}</h2>
        {entry && (
          <div className="preview-actions">
            {savedFlash && text === saved && (
              <span className="notes-save-state">Сохранено</span>
            )}
            {text !== saved && <span className="notes-save-state dirty">Не сохранено</span>}
            {entry.isMarkdown && (
              <button
                className="ghost-btn small"
                onClick={() => setMode(mode === "source" ? "blocks" : "source")}
              >
                {mode === "source" ? "Готово" : "Исходник"}
              </button>
            )}
            {textual && (
              <button className="ghost-btn small" onClick={() => void openHistory()}>
                История версий
              </button>
            )}
            {!textual && (
              <button className="ghost-btn small" onClick={() => void openInSystem()}>
                Открыть в системе
              </button>
            )}
          </div>
        )}
      </div>
      {error && <div className="tree-error notes-editor-error">{error}</div>}
      {conflict && (
        <div className="preview-conflict notes-conflict" role="alert">
          <span className="preview-conflict-text">Файл изменён извне</span>
          <div className="preview-conflict-actions">
            <button
              className="ghost-btn small"
              onClick={() => external !== null && applyExternal(external)}
            >
              Обновить
            </button>
            <button
              className="ghost-btn small"
              onClick={() => {
                setConflict(false);
                setExternal(null);
              }}
            >
              Оставить моё
            </button>
          </div>
        </div>
      )}
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
            <h1>Заметки и файлы</h1>
            <p>Выберите заметку или файл слева или создайте новую</p>
          </div>
        </div>
      ) : loading ? (
        <div className="center-body">Загрузка…</div>
      ) : missing ? (
        <div className="center-body">
          <div className="preview-body preview-error">
            Файл недоступен — он был удалён или переименован.
          </div>
        </div>
      ) : !entry.isMarkdown && entry.isText ? (
        <div className="notes-editor-body">
          <pre ref={textPreRef} className="preview-body">
            {text}
          </pre>
        </div>
      ) : !entry.isMarkdown && entry.isImage ? (
        <div className="notes-editor-body">
          <div className="preview-body preview-image">
            {dataUrl && <img src={dataUrl} alt={entry.name} />}
          </div>
        </div>
      ) : !entry.isMarkdown && isPdf ? (
        <div className="notes-editor-body">
          <div className="preview-body preview-pdf">
            {pdfUrl && <iframe src={pdfUrl} title={entry.name} />}
          </div>
        </div>
      ) : !entry.isMarkdown ? (
        <div className="notes-editor-body">
          <div className="notes-file-card">
            <span className="file-card-icon">
              <EntryIcon entry={entry} />
            </span>
            <span className="file-card-name">{entry.name}</span>
            <span className="file-card-meta">
              {formatSize(entry.size)}
              {entry.ext !== "" && ` · ${entry.ext.toUpperCase()}`}
            </span>
          </div>
        </div>
      ) : mode === "source" ? (
        <div className="notes-editor-body">
          <MarkdownCodeEditor
            value={text}
            onChange={setText}
            autoFocus
            ariaLabel="Исходный Markdown заметки"
            viewRef={sourceViewRef}
          />
        </div>
      ) : (
        <div ref={blocksRef} className="notes-editor-body notes-blocks">
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
                  dangerouslySetInnerHTML={{ __html: renderMarkdown(block.text, { breaks: true }) }}
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
      {historyAbs !== null && entry && (
        <VersionHistory
          scope="notes"
          absPath={historyAbs}
          onRestore={restoreVersion}
          onClose={() => setHistoryAbs(null)}
        />
      )}
    </section>
  );
}

import { useEffect, useMemo, useState } from "react";
import { marked } from "marked";
import DOMPurify from "dompurify";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import { toMessage } from "../chat/chatShared";

interface Props {
  entry: FsEntry | null;
  onDirtyChange: (dirty: boolean) => void;
}

export default function NoteEditor({ entry, onDirtyChange }: Props) {
  const [mode, setMode] = useState<"view" | "edit">("view");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [saved, setSaved] = useState("");

  useEffect(() => {
    setMode("view");
    setError(null);
    setText("");
    setSaved("");
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
    onDirtyChange(entry != null && mode === "edit" && text !== saved);
  }, [entry, mode, text, saved, onDirtyChange]);

  const html = useMemo(
    () => (entry?.isMarkdown ? DOMPurify.sanitize(marked.parse(text, { async: false })) : null),
    [entry?.isMarkdown, text],
  );

  const toggleMode = () => {
    if (mode === "view") {
      setMode("edit");
      return;
    }
    if (!entry) return;
    void ipc
      .notesWrite(entry.relPath, text)
      .then(() => {
        setSaved(text);
        setMode("view");
        setError(null);
      })
      .catch((e) => setError(toMessage(e)));
  };

  return (
    <section className="center">
      <div className="center-head">
        <h2>{entry ? entry.relPath : "Блокнот"}</h2>
        {entry && (
          <div className="preview-actions">
            <button className="ghost-btn small" onClick={() => void toggleMode()}>
              {mode === "view" ? "Редактировать" : "Готово"}
            </button>
          </div>
        )}
      </div>
      {error && <div className="tree-error notes-editor-error">{error}</div>}
      {!entry ? (
        <div className="center-body">
          <div className="placeholder">
            <h1>Блокнот</h1>
            <p>Выберите заметку слева или создайте новую</p>
          </div>
        </div>
      ) : loading ? (
        <div className="center-body">Загрузка…</div>
      ) : mode === "edit" ? (
        <div className="notes-editor-body">
          <textarea
            className="preview-textarea"
            value={text}
            onChange={(e) => setText(e.target.value)}
            autoFocus
          />
        </div>
      ) : entry.isMarkdown ? (
        <div className="notes-editor-body">
          <div className="preview-body md" dangerouslySetInnerHTML={{ __html: html ?? "" }} />
        </div>
      ) : (
        <div className="notes-editor-body">
          <pre className="preview-body">{text}</pre>
        </div>
      )}
    </section>
  );
}

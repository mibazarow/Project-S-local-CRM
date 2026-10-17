import { useEffect, useMemo, useState } from "react";
import { marked } from "marked";
import DOMPurify from "dompurify";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import { toMessage } from "./fileOps";
import { IconClose } from "./icons";

const MAX_IMAGE_BYTES = 20 * 1024 * 1024;

interface Props {
  projectId: number;
  entry: FsEntry;
  onClose: () => void;
  onDirtyChange: (dirty: boolean) => void;
}

export default function FilePreview({ projectId, entry, onClose, onDirtyChange }: Props) {
  const isTextual = entry.isMarkdown || entry.isText;
  const [mode, setMode] = useState<"view" | "edit">("view");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [saved, setSaved] = useState("");
  const [dataUrl, setDataUrl] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setMode("view");
    setError(null);
    setText("");
    setSaved("");
    setDataUrl(null);
    if (!isTextual && !entry.isImage) return;
    setLoading(true);
    void (async () => {
      try {
        if (entry.isImage) {
          if (entry.size > MAX_IMAGE_BYTES) {
            if (!cancelled)
              setError(`Файл слишком большой для предпросмотра (лимит ${MAX_IMAGE_BYTES / (1024 * 1024)} МБ)`);
          } else {
            const url = await ipc.fsReadBinaryDataUrl(projectId, entry.relPath, MAX_IMAGE_BYTES);
            if (!cancelled) setDataUrl(url);
          }
        } else {
          const content = await ipc.fsReadText(projectId, entry.relPath);
          if (!cancelled) {
            setText(content);
            setSaved(content);
          }
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
  }, [projectId, entry.relPath, entry.isImage, entry.isMarkdown, entry.isText, entry.size]);

  useEffect(() => {
    onDirtyChange(isTextual && mode === "edit" && text !== saved);
  }, [isTextual, mode, text, saved, onDirtyChange]);

  const html = useMemo(
    () =>
      entry.isMarkdown
        ? DOMPurify.sanitize(marked.parse(text, { async: false }))
        : null,
    [entry.isMarkdown, text],
  );

  const save = async () => {
    try {
      await ipc.fsWriteText(projectId, entry.relPath, text);
      setSaved(text);
      setMode("view");
      setError(null);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const openExternal = async () => {
    try {
      await ipc.fsOpenExternal(projectId, entry.relPath);
      setError(null);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  return (
    <div className="file-preview">
      <div className="preview-head">
        <span>{entry.relPath}</span>
        <div className="preview-actions">
          {isTextual && (
            <button
              className="ghost-btn small"
              onClick={() => (mode === "view" ? setMode("edit") : void save())}
            >
              {mode === "view" ? "Редактировать" : "Готово"}
            </button>
          )}
          {!isTextual && !entry.isImage && (
            <button className="ghost-btn small" onClick={() => void openExternal()}>
              Открыть вовне
            </button>
          )}
          <button className="icon-btn small" title="Закрыть" onClick={onClose}>
            <IconClose />
          </button>
        </div>
      </div>
      {loading ? (
        <div className="preview-body">Загрузка…</div>
      ) : error ? (
        <div className="preview-body preview-error">{error}</div>
      ) : entry.isImage ? (
        <div className="preview-body preview-image">
          {dataUrl && <img src={dataUrl} alt={entry.name} />}
        </div>
      ) : mode === "edit" ? (
        <textarea
          className="preview-textarea"
          value={text}
          onChange={(e) => setText(e.target.value)}
          autoFocus
        />
      ) : entry.isMarkdown ? (
        <div className="preview-body md" dangerouslySetInnerHTML={{ __html: html ?? "" }} />
      ) : (
        <pre className="preview-body">{text}</pre>
      )}
    </div>
  );
}

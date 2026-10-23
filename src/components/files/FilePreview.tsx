import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { marked } from "marked";
import DOMPurify from "dompurify";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import { toMessage } from "./fileOps";
import Button from "../shared/Button";
import IconButton from "../shared/IconButton";
import { useFileCrossActions } from "../shared/crossActions";
import { useOpenWith } from "./useOpenWith";
import { IconClose } from "./icons";
import VersionHistory from "../shared/VersionHistory";

const MAX_IMAGE_BYTES = 20 * 1024 * 1024;
// Must stay in sync with PDF_MAX_BYTES in src-tauri/src/fs_ops.rs.
const MAX_PDF_BYTES = 25 * 1024 * 1024;

const dataUrlToBytes = (dataUrl: string): Uint8Array<ArrayBuffer> => {
  const bin = atob(dataUrl.slice(dataUrl.indexOf(",") + 1));
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
};

interface Props {
  projectId: number;
  entry: FsEntry;
  onClose: () => void;
  onDirtyChange: (dirty: boolean) => void;
}

export default function FilePreview({ projectId, entry, onClose, onDirtyChange }: Props) {
  const isTextual = entry.isMarkdown || entry.isText;
  const isPdf = entry.ext === "pdf";
  const ow = useOpenWith(projectId);
  const cross = useFileCrossActions(projectId);
  const [mode, setMode] = useState<"view" | "edit">("view");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [saved, setSaved] = useState("");
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  const [pdfUrl, setPdfUrl] = useState<string | null>(null);
  const [historyAbs, setHistoryAbs] = useState<string | null>(null);
  const [reloadTick, setReloadTick] = useState(0);
  // External-change reload: disk content differs from the last loaded version.
  const [external, setExternal] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const [missing, setMissing] = useState(false);
  const [reloadSeq, setReloadSeq] = useState(0);
  const bodyRef = useRef<HTMLElement | null>(null);
  const restoreRef = useRef<{ top: number; start: number | null; end: number | null } | null>(null);
  const savedRef = useRef("");
  const dirtyRef = useRef(false);

  useEffect(() => {
    let cancelled = false;
    let objectUrl: string | null = null;
    setMode("view");
    setError(null);
    setText("");
    setSaved("");
    setDataUrl(null);
    setPdfUrl(null);
    setExternal(null);
    setConflict(false);
    setMissing(false);
    if (!isTextual && !entry.isImage && !isPdf) return;
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
        } else if (isPdf) {
          if (entry.size > MAX_PDF_BYTES) {
            if (!cancelled)
              setError(`Файл слишком большой для предпросмотра (лимит ${MAX_PDF_BYTES / (1024 * 1024)} МБ)`);
          } else {
            const pdfDataUrl = await ipc.fsReadBinaryDataUrl(projectId, entry.relPath, MAX_PDF_BYTES);
            objectUrl = URL.createObjectURL(
              new Blob([dataUrlToBytes(pdfDataUrl)], { type: "application/pdf" }),
            );
            if (cancelled) URL.revokeObjectURL(objectUrl);
            else setPdfUrl(objectUrl);
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
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [projectId, entry.relPath, entry.isImage, entry.isMarkdown, entry.isText, entry.ext, entry.size, isPdf, reloadTick]);

  const dirty = isTextual && mode === "edit" && text !== saved;

  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);

  savedRef.current = saved;
  dirtyRef.current = dirty;

  // Replace the shown content with freshly read disk content, keeping the
  // scroll position and (in edit mode) the caret/selection of the body element.
  const applyExternal = useCallback((content: string) => {
    const el = bodyRef.current;
    if (el) {
      const ta = el instanceof HTMLTextAreaElement ? el : null;
      restoreRef.current = {
        top: el.scrollTop,
        start: ta ? ta.selectionStart : null,
        end: ta ? ta.selectionEnd : null,
      };
    }
    setText(content);
    setSaved(content);
    setExternal(null);
    setConflict(false);
    setMissing(false);
    setReloadSeq((s) => s + 1);
  }, []);

  useLayoutEffect(() => {
    if (reloadSeq === 0) return;
    const el = bodyRef.current;
    const r = restoreRef.current;
    restoreRef.current = null;
    if (!el || !r) return;
    el.scrollTop = r.top;
    if (r.start !== null && el instanceof HTMLTextAreaElement) {
      el.focus();
      el.setSelectionRange(r.start, r.end ?? r.start);
    }
  }, [reloadSeq]);

  // Re-read the open file when the backend reports a change for this path
  // (fs-changed from our own/agent writes, index-changed from the watcher for
  // edits by another program). Content is diffed against the last loaded
  // version, so our own saves and no-op events never loop or raise a banner.
  const checkExternal = useCallback(() => {
    void (async () => {
      let disk: string;
      try {
        disk = await ipc.fsReadText(projectId, entry.relPath);
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
  }, [projectId, entry.relPath, applyExternal]);

  useEffect(() => {
    if (!isTextual) return;
    let cancelled = false;
    let unFs: (() => void) | undefined;
    let unIndex: (() => void) | undefined;
    void ipc
      .onFsChanged((p) => {
        if (!cancelled && p.projectId === projectId && p.relPath === entry.relPath) checkExternal();
      })
      .then((u) => (cancelled ? u() : (unFs = u)));
    void ipc
      .onIndexChanged((p) => {
        if (!cancelled && p.projectId === projectId) checkExternal();
      })
      .then((u) => (cancelled ? u() : (unIndex = u)));
    return () => {
      cancelled = true;
      unFs?.();
      unIndex?.();
    };
  }, [projectId, entry.relPath, isTextual, checkExternal]);

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

  // Version snapshots are keyed by the absolute path, so resolve the project
  // root and join the project-relative path before opening the panel.
  const openHistory = async () => {
    try {
      const root = await ipc.projectPath(projectId);
      setHistoryAbs(`${root}/${entry.relPath}`);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const restoreVersion = async (content: string) => {
    await ipc.fsWriteText(projectId, entry.relPath, content);
    setReloadTick((t) => t + 1);
  };

  const shownError = error ?? ow.error;

  return (
    <div className="file-preview">
      <div className="preview-head">
        <span>{entry.relPath}</span>
        <div className="preview-actions">
          <Button variant="ghost" size="small" title="Спросить агента о файле" onClick={() => cross.askAgent(entry)}>
            Спросить агента
          </Button>
          {isTextual && (
            <Button
              variant="ghost"
              size="small"
              onClick={() => (mode === "view" ? setMode("edit") : void save())}
            >
              {mode === "view" ? "Редактировать" : "Готово"}
            </Button>
          )}
          {isTextual && (
            <Button variant="ghost" size="small" onClick={() => void openHistory()}>
              История версий
            </Button>
          )}
          {!isTextual && !entry.isImage && (
            <Button variant="ghost" size="small" onClick={() => void ow.showChooser(entry)}>
              Открыть в редакторе
            </Button>
          )}
          <IconButton size="small" title="Закрыть" onClick={onClose}>
            <IconClose />
          </IconButton>
        </div>
      </div>
      {conflict && (
        <div className="preview-conflict" role="alert">
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
      {loading ? (
        <div className="preview-body">Загрузка…</div>
      ) : missing ? (
        <div className="preview-body preview-error">
          Файл недоступен — он был удалён или переименован.
        </div>
      ) : shownError ? (
        <div className="preview-body preview-error">
          {shownError}
          {isPdf && (
            <div className="preview-error-actions">
              <button className="ghost-btn small" onClick={() => void ow.showChooser(entry)}>
                Открыть в редакторе
              </button>
            </div>
          )}
        </div>
      ) : isPdf ? (
        <div className="preview-body preview-pdf">
          {pdfUrl && <iframe src={pdfUrl} title={entry.name} />}
        </div>
      ) : entry.isImage ? (
        <div className="preview-body preview-image">
          {dataUrl && <img src={dataUrl} alt={entry.name} />}
        </div>
      ) : mode === "edit" ? (
        <textarea
          ref={(el) => {
            bodyRef.current = el;
          }}
          className="preview-textarea"
          value={text}
          onChange={(e) => setText(e.target.value)}
          autoFocus
        />
      ) : entry.isMarkdown ? (
        <div
          ref={(el) => {
            bodyRef.current = el;
          }}
          className="preview-body md"
          dangerouslySetInnerHTML={{ __html: html ?? "" }}
        />
      ) : (
        <pre
          ref={(el) => {
            bodyRef.current = el;
          }}
          className="preview-body"
        >
          {text}
        </pre>
      )}
      {ow.dialog}
      {historyAbs !== null && (
        <VersionHistory
          scope="project"
          absPath={historyAbs}
          onRestore={restoreVersion}
          onClose={() => setHistoryAbs(null)}
        />
      )}
    </div>
  );
}

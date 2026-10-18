import { useCallback, useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type { VersionDto } from "../../api/types";
import { IconClose } from "../files/icons";

interface Props {
  scope: "project" | "notes";
  // Absolute path the snapshots are keyed by (matches the backend version key).
  absPath: string;
  // Writes the restored content back through the scope's own write command.
  onRestore: (content: string) => Promise<void>;
  onClose: () => void;
}

function errMsg(e: unknown): string {
  if (e && typeof e === "object" && "message" in e) {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string" && m) return m;
  }
  return typeof e === "string" ? e : "Неизвестная ошибка";
}

// SQLite "YYYY-MM-DD HH:MM:SS" (localtime) → drop the seconds for display.
function fmtDate(s: string): string {
  return s.length >= 16 ? s.slice(0, 16) : s;
}

export default function VersionHistory({ scope, absPath, onRestore, onClose }: Props) {
  const [versions, setVersions] = useState<VersionDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<number | null>(null);

  const load = useCallback(() => {
    void ipc
      .versionList(scope, absPath)
      .then((v) => {
        setVersions(v);
        setError(null);
      })
      .catch((e) => setError(errMsg(e)));
  }, [scope, absPath]);

  useEffect(() => {
    load();
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void ipc
      .onVersionsChanged((p) => {
        if (p.scope === scope && p.path === absPath) load();
      })
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [load, scope, absPath]);

  const restore = async (id: number) => {
    if (!window.confirm("Заменить текущее содержимое этой версией?")) return;
    setBusyId(id);
    try {
      const r = await ipc.versionRestore(id);
      await onRestore(r.content);
      load();
    } catch (e) {
      setError(errMsg(e));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <div
      className="modal-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal version-modal">
        <button className="icon-btn small settings-close" title="Закрыть" onClick={onClose}>
          <IconClose />
        </button>
        <h3>История версий</h3>
        <div className="set-hint version-path">{absPath}</div>
        {error && <div className="set-error">{error}</div>}
        {versions === null ? (
          <div className="set-hint">Загрузка…</div>
        ) : versions.length === 0 ? (
          <div className="set-hint">
            Версий пока нет. Предыдущее содержимое сохраняется при правках заметок и действий агента.
          </div>
        ) : (
          <div className="skill-list version-list">
            {versions.map((v) => (
              <div key={v.id} className="skill-item">
                <div className="info">
                  <div className="t">{fmtDate(v.createdAt)}</div>
                  <div className="c">{v.bytes.toLocaleString("ru-RU")} Б</div>
                </div>
                <span className="version-actions">
                  <button
                    className="ghost-btn small"
                    disabled={busyId !== null}
                    onClick={() => void restore(v.id)}
                  >
                    {busyId === v.id ? "Восстановление…" : "Восстановить"}
                  </button>
                </span>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

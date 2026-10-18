import { useEffect, useState } from "react";
import type { FsEntry, OpenWithApp } from "../../api/types";
import { toMessage } from "./fileOps";
import { IconClose } from "./icons";

interface Props {
  entry: FsEntry;
  apps: OpenWithApp[];
  // app = null means "Открыть по умолчанию" (system default).
  onPick: (app: OpenWithApp | null, remember: boolean) => Promise<void>;
  onClose: () => void;
}

export default function OpenWithDialog({ entry, apps, onPick, onClose }: Props) {
  const [remember, setRemember] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const pick = async (app: OpenWithApp | null) => {
    setBusy(true);
    try {
      await onPick(app, remember);
    } catch (e) {
      setError(toMessage(e));
      setBusy(false);
    }
  };

  return (
    <div
      className="modal-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal open-with-modal">
        <div className="open-with-head">
          <h3>Открыть «{entry.name}» с помощью…</h3>
          <button className="icon-btn small" title="Закрыть" onClick={onClose}>
            <IconClose />
          </button>
        </div>
        {error && <div className="set-error">{error}</div>}
        {apps.length === 0 ? (
          <div className="open-with-empty">Приложений кроме стандартных не найдено.</div>
        ) : (
          <div className="open-with-list">
            {apps.map((app) => (
              <button
                key={app.path}
                className="open-with-item"
                disabled={busy}
                onClick={() => void pick(app)}
              >
                <span className="ow-name">{app.name}</span>
                {app.recommended && <span className="ow-badge">рекомендуется</span>}
              </button>
            ))}
          </div>
        )}
        <label className="ow-remember">
          <input
            type="checkbox"
            checked={remember}
            onChange={(e) => setRemember(e.target.checked)}
            disabled={busy}
          />
          Запомнить выбор
        </label>
        <div className="open-with-actions">
          <button className="ghost-btn small" disabled={busy} onClick={() => void pick(null)}>
            Открыть по умолчанию
          </button>
        </div>
      </div>
    </div>
  );
}

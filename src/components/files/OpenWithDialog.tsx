import { useState } from "react";
import type { FsEntry, OpenWithApp } from "../../api/types";
import { toMessage } from "./fileOps";
import { IconClose } from "./icons";
import Modal from "../shared/Modal";

interface Props {
  entry: FsEntry;
  apps: OpenWithApp[];
  rememberedPath?: string | null;
  // app = null means "Открыть по умолчанию" (system default).
  onPick: (app: OpenWithApp | null, remember: boolean) => Promise<void>;
  onClose: () => void;
}

export default function OpenWithDialog({ entry, apps, rememberedPath, onPick, onClose }: Props) {
  const [remember, setRemember] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
    <Modal onClose={onClose} closeOnEsc className="modal open-with-modal">
      <div className="open-with-head">
        <h3>Открыть «{entry.name}» в редакторе</h3>
        <button className="icon-btn small" title="Закрыть" onClick={onClose}>
          <IconClose />
        </button>
      </div>
      {error && <div className="set-error">{error}</div>}
      {apps.length === 0 ? (
        <div className="open-with-empty">
          Не найдено приложений, которые умеют открывать этот формат. Откроется в
          приложении по умолчанию — нажмите «Открыть по умолчанию».
        </div>
      ) : (
        <div className="open-with-list">
          {apps.map((app) => (
            <button
              key={app.path}
              className={app.path === rememberedPath ? "open-with-item remembered" : "open-with-item"}
              disabled={busy}
              onClick={() => void pick(app)}
            >
              <span className="ow-name">{app.name}</span>
              {app.path === rememberedPath && <span className="ow-badge remembered">запомнено</span>}
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
    </Modal>
  );
}

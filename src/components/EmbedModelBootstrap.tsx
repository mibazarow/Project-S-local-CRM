import { useEffect } from "react";
import { useApp } from "../state/AppStore";
import {
  dismissEmbedModelError,
  startEmbedModelDownload,
  useEmbedModelDownload,
} from "../state/embedModelDownload";

// Non-blocking bottom-left banner for the one-time embed model download:
// progress while downloading, retry/dismiss on error. Success is reported
// through the toast channel by the store itself.
export default function EmbedModelBootstrap() {
  const { state } = useApp();
  const dl = useEmbedModelDownload();
  const configured = state.onboarding.configured;

  useEffect(() => {
    if (!configured) return;
    void startEmbedModelDownload();
  }, [configured]);

  if (dl.phase === "downloading") {
    const pct = dl.total > 0 ? Math.min(100, Math.round((dl.done / dl.total) * 100)) : 0;
    return (
      <div className="embed-model-banner" role="status" aria-live="polite">
        <div className="embed-model-banner-title">Скачиваем модель для поиска по смыслу…</div>
        <div className="embed-model-banner-meta">
          ~300 МБ · {pct}%{dl.file && ` · ${dl.file}`}
        </div>
        <div className="embed-model-progress" aria-hidden="true">
          <div className="embed-model-progress-fill" style={{ width: `${pct}%` }} />
        </div>
      </div>
    );
  }

  if (dl.phase === "error") {
    return (
      <div className="embed-model-banner error" role="alert">
        <div className="embed-model-banner-title">
          Не удалось скачать модель. Поиск по словам работает; векторный можно включить позже.
        </div>
        <div className="embed-model-banner-actions">
          <button className="ghost-btn" onClick={() => void startEmbedModelDownload()}>
            Повторить
          </button>
          <button className="ghost-btn" onClick={dismissEmbedModelError}>
            Скрыть
          </button>
        </div>
      </div>
    );
  }

  return null;
}

import { useEffect, useState } from "react";
import * as ipc from "../api/ipc";
import { useApp } from "../state/AppStore";
import type { UpdateInfo } from "../api/types";

const DISMISS_KEY = "project-s.update.dismissed";
const CHECK_INTERVAL_MS = 8 * 60 * 60 * 1000;

export default function UpdateBanner() {
  const { state } = useApp();
  const [info, setInfo] = useState<UpdateInfo | null>(null);

  useEffect(() => {
    if (!state.onboarding.configured) return;
    let cancelled = false;
    const check = () => {
      void ipc
        .checkUpdate()
        .then((next) => {
          if (cancelled) return;
          if (!next.available || !next.latest || !next.url) {
            setInfo(null);
            return;
          }
          if (localStorage.getItem(DISMISS_KEY) === next.latest) {
            setInfo(null);
            return;
          }
          setInfo(next);
        })
        .catch(() => {
          // An update check must never interrupt the app.
        });
    };
    check();
    const timer = window.setInterval(check, CHECK_INTERVAL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [state.onboarding.configured]);

  if (!info?.latest || !info.url) return null;

  const dismiss = () => {
    localStorage.setItem(DISMISS_KEY, info.latest!);
    setInfo(null);
  };

  return (
    <div className="update-banner" role="status" aria-live="polite">
      <button
        type="button"
        className="update-banner-close"
        aria-label="Скрыть уведомление"
        onClick={dismiss}
      >
        ×
      </button>
      <div className="update-banner-title">Доступна новая версия Project-S {info.latest}</div>
      <div className="update-banner-text">
        Установите её поверх текущей версии — проекты, заметки и настройки сохранятся.
      </div>
      <a className="update-banner-link" href={info.url}>
        Релизы
      </a>
    </div>
  );
}

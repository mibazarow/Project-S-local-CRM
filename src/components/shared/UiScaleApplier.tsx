import { useEffect } from "react";
import * as ipc from "../../api/ipc";
import { applyUiScale, normalizeUiScale } from "../../shared/uiScale";

// Applies the saved UI scale from app.json once at startup, before the main
// views paint. Live changes are applied directly by the settings control; this
// component only covers the initial render and keeps the zoom in sync when the
// app config is reloaded from disk. Renders nothing.
export default function UiScaleApplier() {
  useEffect(() => {
    let cancelled = false;
    const apply = () => {
      void ipc
        .readConfig("app")
        .then((raw) => {
          if (cancelled) return;
          const percent = normalizeUiScale(
            (raw as { uiScalePercent?: unknown } | null)?.uiScalePercent,
          );
          void applyUiScale(percent);
        })
        .catch(() => {
          // No working dir yet (onboarding) or read failed: keep the default.
        });
    };
    apply();
    let unlisten: (() => void) | undefined;
    void ipc
      .onConfigChanged(({ which }) => {
        if (which === "app") apply();
      })
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);
  return null;
}

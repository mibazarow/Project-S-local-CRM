import { useEffect, useState } from "react";
import * as ipc from "../../../api/ipc";

// Vision support for the composer warning: cached/override status read once per
// chat mount and refreshed whenever llm.json changes. Returns the effective
// `supported` flag (null = unknown / never probed).
export function useVisionSupport(): boolean | null {
  const [supported, setSupported] = useState<boolean | null>(null);
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    const load = () =>
      void ipc
        .getVisionStatus()
        .then((s) => !cancelled && setSupported(s.supported))
        .catch(() => {});
    load();
    void ipc
      .onConfigChanged(({ which }) => {
        if (which === "llm") load();
      })
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);
  return supported;
}

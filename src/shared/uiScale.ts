import { getCurrentWebview } from "@tauri-apps/api/webview";
import { toMessage } from "./errors";

export const UI_SCALE_STEPS = [100, 110, 130, 175];
export const DEFAULT_UI_SCALE_PERCENT = 100;

let lastError: string | null = null;
const listeners = new Set<() => void>();

function setLastError(message: string | null): void {
  if (lastError === message) return;
  lastError = message;
  for (const listener of listeners) listener();
}

export function getUiScaleError(): string | null {
  return lastError;
}

export function subscribeUiScaleError(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function normalizeUiScale(value: unknown): number {
  const n = typeof value === "number" ? value : Number(value);
  return UI_SCALE_STEPS.includes(n) ? n : DEFAULT_UI_SCALE_PERCENT;
}

export function uiScaleLabel(percent: number): string {
  return `${normalizeUiScale(percent)} %`;
}

// Applies the whole-UI scale live via the Tauri 2 webview zoom. The webview
// zoom is the single scaling mechanism (no CSS root scale is applied on top, so
// the two never double-apply). A rejected zoom (for example a missing ACL
// permission) is logged and recorded so Settings can show it.
export async function applyUiScale(percent: number): Promise<boolean> {
  const zoom = normalizeUiScale(percent) / 100;
  try {
    await getCurrentWebview().setZoom(zoom);
    setLastError(null);
    return true;
  } catch (e) {
    console.error("ui scale: setZoom failed", e);
    setLastError(toMessage(e));
    return false;
  }
}

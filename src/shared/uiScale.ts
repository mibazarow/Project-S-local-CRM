import { getCurrentWebview } from "@tauri-apps/api/webview";

// The only UI scale steps the settings control offers (Q.md item 4). Anything
// else read from app.json normalizes to the default 100.
export const UI_SCALE_STEPS = [100, 110, 125, 150, 175, 200];
export const DEFAULT_UI_SCALE_PERCENT = 100;

export function normalizeUiScale(value: unknown): number {
  const n = typeof value === "number" ? value : Number(value);
  return UI_SCALE_STEPS.includes(n) ? n : DEFAULT_UI_SCALE_PERCENT;
}

export function uiScaleLabel(percent: number): string {
  return `${normalizeUiScale(percent)} %`;
}

// Applies the whole-UI scale live via the Tauri 2 webview zoom. The webview
// zoom is the single scaling mechanism (no CSS root scale is applied on top, so
// the two never double-apply). Outside Tauri (dev/tests) the API is unavailable
// and the call degrades to a no-op, leaving the default scale.
export async function applyUiScale(percent: number): Promise<void> {
  const zoom = normalizeUiScale(percent) / 100;
  try {
    await getCurrentWebview().setZoom(zoom);
  } catch {
    // Webview zoom unavailable: keep the current scale.
  }
}

import type { AppConfig, IndexingConfig, LlmConfig, VisionStatus } from "../../api/types";
import { normalizeUiScale } from "../../shared/uiScale";

export const MB = 1024 * 1024;

// Form fields are kept as strings so mid-typing states ("20.", "") survive;
// parsing happens only at save time. Semantic search settings are not part of
// the form: semantic search is always on and only its indexing limits are here.
export interface Form {
  maxMb: string;
  maxChars: string;
  excludeDirs: string;
}

export function toForm(cfg: IndexingConfig): Form {
  return {
    maxMb: String(Math.round((cfg.maxFileSizeBytes / MB) * 100) / 100),
    maxChars: String(cfg.maxTextChars),
    excludeDirs: cfg.excludeDirs.join(", "),
  };
}

export function parseNum(s: string): number | null {
  const n = Number(s.replace(",", "."));
  return Number.isFinite(n) ? n : null;
}

// Read-modify-write patch for indexing.json: starts from the last raw read so
// keys the app no longer owns (e.g. a stale `embedding` object in old files)
// are left untouched on disk; the backend ignores them.
export function indexingToPatch(f: Form, raw: Record<string, unknown>): Record<string, unknown> {
  const patch: Record<string, unknown> = { ...raw };
  const mb = parseNum(f.maxMb);
  if (mb !== null) patch.maxFileSizeBytes = Math.round(mb * MB);
  const chars = parseNum(f.maxChars);
  if (chars !== null) patch.maxTextChars = Math.round(chars);
  patch.excludeDirs = f.excludeDirs
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  return patch;
}

export interface LlmForm {
  model: string;
  lastModelByProvider: Record<string, string>;
  contextWindow: string;
  modelContextWindows: Record<string, number>;
  modelContextMax: Record<string, number>;
  maxTokens: string;
  vision: "auto" | "yes" | "no";
  thinkingLevel: "off" | "low" | "medium" | "high";
}

export function toLlmForm(cfg: LlmConfig): LlmForm {
  const windows = cfg.modelContextWindows ?? {};
  const max = cfg.modelContextMax ?? {};
  const current = windows[cfg.model] ?? cfg.contextWindow;
  return {
    model: cfg.model,
    lastModelByProvider: cfg.lastModelByProvider ?? {},
    contextWindow: String(clampContextWindow(current, max[cfg.model])),
    modelContextWindows: windows,
    modelContextMax: max,
    maxTokens: cfg.maxTokens == null ? "" : String(cfg.maxTokens),
    vision: cfg.vision == null ? "auto" : cfg.vision ? "yes" : "no",
    thinkingLevel: cfg.thinkingLevel ?? "medium",
  };
}

// Read-modify-write patch for llm.json (same unknown-key rule as indexing).
// baseUrl/apiKey are NOT here: they belong to the active provider and are owned
// by the provider commands (which mirror them into the legacy top-level fields).
export function llmToPatch(f: LlmForm, raw: Record<string, unknown>): Record<string, unknown> {
  const patch: Record<string, unknown> = { ...raw };
  patch.model = f.model;
  patch.lastModelByProvider = f.lastModelByProvider;
  const cw = parseNum(f.contextWindow);
  if (cw !== null) {
    const clamped = clampContextWindow(cw, f.modelContextMax[f.model]);
    if (f.model.trim() !== "") {
      patch.modelContextWindows = { ...f.modelContextWindows, [f.model]: clamped };
    }
    patch.contextWindow = clamped;
  }
  patch.modelContextMax = f.modelContextMax;
  if (f.maxTokens.trim() === "") patch.maxTokens = null;
  else {
    const m = parseNum(f.maxTokens);
    if (m !== null) patch.maxTokens = Math.round(m);
  }
  patch.vision = f.vision === "auto" ? null : f.vision === "yes";
  patch.thinkingLevel = f.thinkingLevel;
  return patch;
}

// Provider editor form (Q-24): raw strings, validated/saved via the provider
// commands. `editingId` null means the form adds a new provider.
export interface ProviderForm {
  name: string;
  baseUrl: string;
  apiKey: string;
}

export const emptyProviderForm: ProviderForm = { name: "", baseUrl: "", apiKey: "" };

export function fmtMb(bytes: number): string {
  return (bytes / MB).toFixed(1);
}

// RFC3339 UTC (backup_status.lastBackupAt) → local, human-readable.
export function fmtRfc(s: string): string {
  const d = new Date(s);
  return Number.isNaN(d.getTime()) ? s : d.toLocaleString("ru-RU");
}

// SQLite "YYYY-MM-DD HH:MM:SS" (localtime, trash deleted_at) → drop seconds.
export function fmtLocal(s: string): string {
  return s.length >= 16 ? s.slice(0, 16) : s;
}

// Local timestamp for a suggested export filename (YYYYMMDD-HHMMSS), matching
// the backend's default export name format.
export function exportStamp(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(
    d.getMinutes(),
  )}${p(d.getSeconds())}`;
}

export function visionLabel(v: VisionStatus | null): string {
  if (!v || v.supported == null) return "Не проверено";
  return v.supported ? "Поддерживается" : "Не поддерживается";
}

// app.json fields edited by the "Агент" and "Прочее" tabs share one form so a
// single debounced writer owns the file.
export interface AppForm {
  autopilot: boolean;
  sessionTtlDays: string;
  tmpRetentionDays: string;
  logRetentionDays: string;
  backupIntervalDays: string;
  backupKeep: string;
  uiScalePercent: number;
}

export function toAppForm(cfg: Partial<AppConfig>): AppForm {
  return {
    autopilot: cfg.autopilot ?? false,
    sessionTtlDays: String(cfg.sessionTtlDays ?? 30),
    tmpRetentionDays: String(cfg.tmpRetentionDays ?? 7),
    logRetentionDays: String(cfg.logRetentionDays ?? 14),
    backupIntervalDays: String(cfg.backupIntervalDays ?? 1),
    backupKeep: String(cfg.backupKeep ?? 7),
    uiScalePercent: normalizeUiScale(cfg.uiScalePercent),
  };
}

// Read-modify-write patch for app.json; autostart is owned by set_autostart,
// so its on-disk value is kept untouched.
export function appToPatch(f: AppForm, raw: Record<string, unknown>): Record<string, unknown> {
  const patch: Record<string, unknown> = { ...raw };
  delete patch.autostart;
  patch.autopilot = f.autopilot;
  const ttl = parseNum(f.sessionTtlDays);
  if (ttl !== null) patch.sessionTtlDays = Math.max(0, Math.round(ttl));
  const tmpDays = parseNum(f.tmpRetentionDays);
  if (tmpDays !== null) patch.tmpRetentionDays = Math.max(0, Math.round(tmpDays));
  const logDays = parseNum(f.logRetentionDays);
  if (logDays !== null) patch.logRetentionDays = Math.max(0, Math.round(logDays));
  const bInterval = parseNum(f.backupIntervalDays);
  if (bInterval !== null) patch.backupIntervalDays = Math.max(0, Math.round(bInterval));
  const bKeep = parseNum(f.backupKeep);
  if (bKeep !== null) patch.backupKeep = Math.max(0, Math.round(bKeep));
  patch.uiScalePercent = normalizeUiScale(f.uiScalePercent);
  return patch;
}

export type McpTransportKind = "stdio" | "http" | "sse";

// MCP add/edit form: raw strings, parsed only at save time.
export interface McpForm {
  name: string;
  transport: McpTransportKind;
  command: string;
  args: string;
  env: string;
  url: string;
  headers: string;
  timeout: string;
}

export const emptyMcpForm: McpForm = {
  name: "",
  transport: "stdio",
  command: "",
  args: "",
  env: "",
  url: "",
  headers: "",
  timeout: "",
};

export type SkillKindOption = "instruction" | "script" | "extension";

// "Создать скилл" form; values map 1:1 onto the skills_create command.
export interface SkillForm {
  name: string;
  description: string;
  kind: SkillKindOption;
  body: string;
  overwrite: boolean;
}

export const emptySkillForm: SkillForm = {
  name: "",
  description: "",
  kind: "instruction",
  body: "",
  overwrite: false,
};

// KEY=VALUE (or KEY: VALUE when allowColon) lines → record; null on a
// malformed line so the form can show an inline error.
export function parseKeyLines(s: string, allowColon: boolean): Record<string, string> | null {
  const out: Record<string, string> = {};
  for (const line of s.split("\n")) {
    const t = line.trim();
    if (!t) continue;
    const m = allowColon ? t.match(/^([^=:]+)[=:](.*)$/) : t.match(/^([^=]+)=(.*)$/);
    if (!m || !m[1].trim()) return null;
    out[m[1].trim()] = m[2].trim();
  }
  return out;
}

export const DEFAULT_CONTEXT_WINDOW = 100_000;
export const MAX_UNREPORTED_CONTEXT_WINDOW = 250_000;

export function contextWindowLimit(max: number | null | undefined): number {
  return max != null && max > 0 ? max : MAX_UNREPORTED_CONTEXT_WINDOW;
}

export function clampContextWindow(value: number, max?: number | null): number {
  return Math.max(1, Math.min(Math.round(value), contextWindowLimit(max)));
}

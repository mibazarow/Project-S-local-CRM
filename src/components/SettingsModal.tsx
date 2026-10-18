import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type FormEvent,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import * as ipc from "../api/ipc";
import type {
  AppConfig,
  BackupStatus,
  ConnectionTest,
  EmbedStatus,
  ImportResult,
  IndexingConfig,
  LlmConfig,
  McpServerStatus,
  ProjectDto,
  SkillInfo,
  TrashDto,
  VisionStatus,
} from "../api/types";

const MB = 1024 * 1024;

// Form fields are kept as strings so mid-typing states ("20.", "") survive;
// parsing happens only at save time. Semantic search settings are not part of
// the form: semantic search is always on and only its indexing limits are here.
interface Form {
  maxMb: string;
  maxChars: string;
  excludeDirs: string;
}

function toForm(cfg: IndexingConfig): Form {
  return {
    maxMb: String(Math.round((cfg.maxFileSizeBytes / MB) * 100) / 100),
    maxChars: String(cfg.maxTextChars),
    excludeDirs: cfg.excludeDirs.join(", "),
  };
}

function parseNum(s: string): number | null {
  const n = Number(s.replace(",", "."));
  return Number.isFinite(n) ? n : null;
}

interface LlmForm {
  baseUrl: string;
  apiKey: string;
  model: string;
  contextWindow: string;
  maxTokens: string;
  vision: "auto" | "yes" | "no";
  thinkingLevel: "off" | "low" | "medium" | "high";
}

function toLlmForm(cfg: LlmConfig): LlmForm {
  return {
    baseUrl: cfg.baseUrl,
    apiKey: cfg.apiKey,
    model: cfg.model,
    contextWindow: String(cfg.contextWindow),
    maxTokens: cfg.maxTokens == null ? "" : String(cfg.maxTokens),
    vision: cfg.vision == null ? "auto" : cfg.vision ? "yes" : "no",
    thinkingLevel: cfg.thinkingLevel ?? "medium",
  };
}

function fmtMb(bytes: number): string {
  return (bytes / MB).toFixed(1);
}

// RFC3339 UTC (backup_status.lastBackupAt) → local, human-readable.
function fmtRfc(s: string): string {
  const d = new Date(s);
  return Number.isNaN(d.getTime()) ? s : d.toLocaleString("ru-RU");
}

// SQLite "YYYY-MM-DD HH:MM:SS" (localtime, trash deleted_at) → drop seconds.
function fmtLocal(s: string): string {
  return s.length >= 16 ? s.slice(0, 16) : s;
}

// Local timestamp for a suggested export filename (YYYYMMDD-HHMMSS), matching
// the backend's default export name format.
function exportStamp(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(
    d.getMinutes(),
  )}${p(d.getSeconds())}`;
}

function visionLabel(v: VisionStatus | null): string {
  if (!v || v.supported == null) return "Не проверено";
  return v.supported ? "Поддерживается" : "Не поддерживается";
}

// app.json fields edited by the "Агент" and "Прочее" tabs share one form so a
// single debounced writer owns the file.
interface AppForm {
  autopilot: boolean;
  sessionTtlDays: string;
  tmpRetentionDays: string;
  logRetentionDays: string;
  backupIntervalDays: string;
  backupKeep: string;
}

function toAppForm(cfg: Partial<AppConfig>): AppForm {
  return {
    autopilot: cfg.autopilot ?? false,
    sessionTtlDays: String(cfg.sessionTtlDays ?? 30),
    tmpRetentionDays: String(cfg.tmpRetentionDays ?? 7),
    logRetentionDays: String(cfg.logRetentionDays ?? 14),
    backupIntervalDays: String(cfg.backupIntervalDays ?? 1),
    backupKeep: String(cfg.backupKeep ?? 7),
  };
}

function errMsg(e: unknown): string {
  if (e && typeof e === "object" && "message" in e) {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string" && m) return m;
  }
  return typeof e === "string" ? e : "Неизвестная ошибка";
}

type McpTransportKind = "stdio" | "http" | "sse";

// MCP add/edit form: raw strings, parsed only at save time.
interface McpForm {
  name: string;
  transport: McpTransportKind;
  command: string;
  args: string;
  env: string;
  url: string;
  headers: string;
}

const emptyMcpForm: McpForm = {
  name: "",
  transport: "stdio",
  command: "",
  args: "",
  env: "",
  url: "",
  headers: "",
};

type SkillKindOption = "instruction" | "script" | "extension";

// "Создать скилл" form; values map 1:1 onto the skills_create command.
interface SkillForm {
  name: string;
  description: string;
  kind: SkillKindOption;
  body: string;
  overwrite: boolean;
}

const emptySkillForm: SkillForm = {
  name: "",
  description: "",
  kind: "instruction",
  body: "",
  overwrite: false,
};

// KEY=VALUE (or KEY: VALUE when allowColon) lines → record; null on a
// malformed line so the form can show an inline error.
function parseKeyLines(s: string, allowColon: boolean): Record<string, string> | null {
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

// Read-modify-write of mcp.json: unknown top-level and per-server keys
// survive, then the servers are restarted and the fresh status returned.
async function patchMcpServers(
  mutate: (servers: Record<string, unknown>[]) => Record<string, unknown>[],
): Promise<McpServerStatus[]> {
  const raw = (await ipc.readConfig("mcp")) as Record<string, unknown>;
  const servers = Array.isArray(raw.servers) ? (raw.servers as Record<string, unknown>[]) : [];
  await ipc.writeConfig("mcp", { ...raw, servers: mutate(servers) });
  return ipc.mcpReload();
}

// Searchable model combobox: free-text id + filtered dropdown of ids loaded
// from the endpoint. ↑/↓ move the highlight, Enter picks it, Esc closes.
interface ModelComboboxProps {
  value: string;
  models: string[] | null;
  onChange: (value: string) => void;
  onFocus: () => void;
  onBlur: () => void;
}

function ModelCombobox({ value, models, onChange, onFocus, onBlur }: ModelComboboxProps) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const listRef = useRef<HTMLDivElement | null>(null);

  const query = value.trim().toLowerCase();
  const filtered = (models ?? []).filter((id) => id.toLowerCase().includes(query));

  useEffect(() => {
    if (!open || active < 0) return;
    const node = listRef.current?.children[active] as HTMLElement | undefined;
    node?.scrollIntoView({ block: "nearest" });
  }, [open, active]);

  const pick = (id: string) => {
    onChange(id);
    setOpen(false);
    setActive(-1);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!open) {
        if (filtered.length > 0) setOpen(true);
        return;
      }
      const next =
        e.key === "ArrowDown"
          ? (active + 1) % filtered.length
          : (active - 1 + filtered.length) % filtered.length;
      setActive(filtered.length ? next : -1);
    } else if (e.key === "Enter") {
      if (open && active >= 0 && active < filtered.length) {
        e.preventDefault();
        pick(filtered[active]);
      } else {
        setOpen(false);
      }
    } else if (e.key === "Escape") {
      e.stopPropagation();
      setOpen(false);
      setActive(-1);
    }
  };

  return (
    <div className="model-combobox">
      <input
        type="text"
        role="combobox"
        aria-expanded={open}
        autoComplete="off"
        spellCheck={false}
        placeholder="gpt-4o-mini или впиши название вручную"
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
          setActive(-1);
        }}
        onFocus={() => {
          onFocus();
          setOpen(true);
          setActive(-1);
        }}
        onBlur={() => {
          onBlur();
          setOpen(false);
          setActive(-1);
        }}
        onKeyDown={onKeyDown}
      />
      {open && (
        <div className="model-dropdown" ref={listRef}>
          {filtered.length === 0 ? (
            <div className="model-empty">
              {models && models.length > 0
                ? "Ничего не найдено — можно вписать вручную"
                : "Список моделей ещё не загружен"}
            </div>
          ) : (
            filtered.map((id, i) => (
              <div
                key={id}
                className={i === active ? "model-option active" : "model-option"}
                // mousedown keeps focus in the input so blur does not close
                // the dropdown before click lands
                onMouseDown={(e) => e.preventDefault()}
                onMouseEnter={() => setActive(i)}
                onClick={() => pick(id)}
              >
                {id}
              </div>
            ))
          )}
        </div>
      )}
    </div>
  );
}

// Context size token in a model id ("gpt-4o-128k", "…-200k", "…-1m"):
// separator + 2+ digits + k/m, so version numbers ("llama3.1", "gpt-4.1")
// never match. Returns the token text and the token count in tokens.
function detectContextToken(model: string): { token: string; tokens: number } | null {
  const m = model.toLowerCase().match(/(?:^|[^a-z0-9])(\d{2,})([km])(?:[^a-z0-9]|$)/);
  if (!m) return null;
  const num = parseInt(m[1], 10);
  return { token: m[0].slice(1), tokens: num * (m[2] === "m" ? 1_000_000 : 1_000) };
}

const DEFAULT_CONTEXT_WINDOW = 100_000;

// Presentational wrappers for the redesigned settings layout: a titled card
// block and a label + control + hint field row. No state, no behavior.
function Card({
  title,
  desc,
  children,
}: {
  title: string;
  desc?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="set-card">
      <div className="set-card-head">
        <div className="set-card-title">{title}</div>
        {desc != null && <div className="set-card-desc">{desc}</div>}
      </div>
      {children}
    </section>
  );
}

function Field({
  label,
  hint,
  error,
  wide,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  wide?: boolean;
  children?: ReactNode;
}) {
  return (
    <div className={wide ? "set-field wide" : "set-field"}>
      <div className="set-field-info">
        <label className="set-field-label">{label}</label>
        {hint != null && <div className="set-field-hint">{hint}</div>}
        {error && <div className="set-field-hint set-field-error">{error}</div>}
      </div>
      {children != null && <div className="set-field-control">{children}</div>}
    </div>
  );
}

export default function SettingsModal({ onClose }: { onClose: () => void }) {
  const [tab, setTab] = useState<"llm" | "agent" | "skills" | "mcp" | "other">("llm");
  const [workingDir, setWorkingDir] = useState<string | null>(null);
  const [configDir, setConfigDir] = useState("");
  const [form, setForm] = useState<Form | null>(null);
  const [embed, setEmbed] = useState<EmbedStatus | null>(null);
  const [reindexBusy, setReindexBusy] = useState(false);
  const [reindexNote, setReindexNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [llmForm, setLlmForm] = useState<LlmForm | null>(null);
  const [llmModels, setLlmModels] = useState<string[] | null>(null);
  const [llmModelsError, setLlmModelsError] = useState<string | null>(null);
  const [llmTest, setLlmTest] = useState<ConnectionTest | null>(null);
  const [llmBusy, setLlmBusy] = useState<"test" | "models" | null>(null);
  const [ctxAutoFilled, setCtxAutoFilled] = useState<{ token: string; tokens: number } | null>(null);
  const [revealKey, setRevealKey] = useState(false);
  const [vision, setVision] = useState<VisionStatus | null>(null);
  const [visionBusy, setVisionBusy] = useState(false);
  const [appForm, setAppForm] = useState<AppForm | null>(null);
  const [backups, setBackups] = useState("");
  const [autostart, setAutostart] = useState<boolean | null>(null);
  const [autostartBusy, setAutostartBusy] = useState(false);
  const [tmpBusy, setTmpBusy] = useState(false);
  const [tmpNote, setTmpNote] = useState<string | null>(null);
  const [backupBusy, setBackupBusy] = useState(false);
  const [backupNote, setBackupNote] = useState<string | null>(null);
  const [backupStatus, setBackupStatus] = useState<BackupStatus | null>(null);
  const [trash, setTrash] = useState<TrashDto[] | null>(null);
  const [trashError, setTrashError] = useState<string | null>(null);
  const [trashBusyId, setTrashBusyId] = useState<number | null>(null);

  // Import/export (Прочее tab): folder/.zip/Obsidian imports into a project or
  // the Notes vault, and project/vault → .zip exports.
  const [projects, setProjects] = useState<ProjectDto[]>([]);
  const [importTarget, setImportTarget] = useState<"project" | "notes">("project");
  const [importProjectId, setImportProjectId] = useState<number | null>(null);
  const [importBusy, setImportBusy] = useState(false);
  const [importNote, setImportNote] = useState<string | null>(null);
  const [importError, setImportError] = useState<string | null>(null);
  const [exportProjectId, setExportProjectId] = useState<number | null>(null);
  const [exportBusy, setExportBusy] = useState(false);
  const [exportNote, setExportNote] = useState<string | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);

  const [skills, setSkills] = useState<SkillInfo[] | null>(null);
  const [skillsError, setSkillsError] = useState<string | null>(null);
  const [skillsBusy, setSkillsBusy] = useState(false);
  const [overwrite, setOverwrite] = useState(false);
  const [gitUrl, setGitUrl] = useState("");
  const [importResults, setImportResults] = useState<ImportResult[] | null>(null);
  const [skillForm, setSkillForm] = useState<SkillForm>({ ...emptySkillForm });
  const [skillFormError, setSkillFormError] = useState<string | null>(null);
  const [skillNote, setSkillNote] = useState<string | null>(null);
  const [skillCreateBusy, setSkillCreateBusy] = useState(false);

  const [mcp, setMcp] = useState<McpServerStatus[] | null>(null);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [mcpBusy, setMcpBusy] = useState(false);
  const [mcpForm, setMcpForm] = useState<McpForm>({ ...emptyMcpForm });
  // Original name of the server being edited; null = the form adds a new one.
  const [mcpEditing, setMcpEditing] = useState<string | null>(null);
  const [mcpFormError, setMcpFormError] = useState<string | null>(null);

  // Last raw config object as read from disk, so unknown keys survive writes.
  const rawRef = useRef<Record<string, unknown> | null>(null);
  const formRef = useRef<Form | null>(null);
  // Name of the field the user is editing; hot-reload never overwrites it.
  const focusedRef = useRef<keyof Form | null>(null);
  const saveTimer = useRef<number | undefined>(undefined);

  const llmRawRef = useRef<Record<string, unknown> | null>(null);
  const llmFormRef = useRef<LlmForm | null>(null);
  const llmFocusedRef = useRef<keyof LlmForm | null>(null);
  const llmSaveTimer = useRef<number | undefined>(undefined);
  // Context window: true once the user typed in the field; auto-fill never
  // overrides a hand-edited value. Reset when the model id itself changes.
  const ctxUserEditedRef = useRef(false);
  const lastModelIdRef = useRef<string | null>(null);
  // One-shot auto-load of the model list once endpoint+key are filled in.
  const modelsAutoLoadedRef = useRef(false);
  const modelsAutoTimer = useRef<number | undefined>(undefined);

  const appRawRef = useRef<Record<string, unknown> | null>(null);
  const appFormRef = useRef<AppForm | null>(null);
  const appFocusedRef = useRef<keyof AppForm | null>(null);
  const appSaveTimer = useRef<number | undefined>(undefined);

  const flushSave = useCallback(() => {
    window.clearTimeout(saveTimer.current);
    const raw = rawRef.current;
    const f = formRef.current;
    if (!raw || !f) return;
    // Read-modify-write: keys the app no longer owns (e.g. a stale
    // `embedding` object in old files) are left untouched on disk; the
    // backend ignores them.
    const patch: Record<string, unknown> = { ...raw };
    const mb = parseNum(f.maxMb);
    if (mb !== null) patch.maxFileSizeBytes = Math.round(mb * MB);
    const chars = parseNum(f.maxChars);
    if (chars !== null) patch.maxTextChars = Math.round(chars);
    patch.excludeDirs = f.excludeDirs
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    void ipc.writeConfig("indexing", patch).catch((e) => setError(errMsg(e)));
  }, []);

  const scheduleSave = useCallback(() => {
    window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(flushSave, 500);
  }, [flushSave]);

  const updateForm = useCallback(
    (patch: Partial<Form>) => {
      if (!formRef.current) return;
      const next = { ...formRef.current, ...patch };
      formRef.current = next;
      setForm(next);
      scheduleSave();
    },
    [scheduleSave],
  );

  // Re-read indexing.json and refresh the form, keeping the focused field as-is
  // so hand-edits to the file show up without clobbering in-progress typing.
  const reloadConfig = useCallback(() => {
    void ipc
      .readConfig("indexing")
      .then((v) => {
        rawRef.current = v as Record<string, unknown>;
        const next = toForm(v as IndexingConfig);
        const focused = focusedRef.current;
        if (focused && formRef.current) {
          (next as unknown as Record<string, unknown>)[focused] = formRef.current[focused];
        }
        formRef.current = next;
        setForm(next);
      })
      .catch((e) => setError(errMsg(e)));
  }, []);

  // Same read-modify-write pattern as indexing, for llm.json.
  const flushLlmSave = useCallback((): Promise<void> => {
    window.clearTimeout(llmSaveTimer.current);
    const raw = llmRawRef.current;
    const f = llmFormRef.current;
    if (!raw || !f) return Promise.resolve();
    const patch: Record<string, unknown> = { ...raw };
    patch.baseUrl = f.baseUrl;
    patch.apiKey = f.apiKey;
    patch.model = f.model;
    const cw = parseNum(f.contextWindow);
    if (cw !== null) patch.contextWindow = Math.max(1, Math.round(cw));
    if (f.maxTokens.trim() === "") patch.maxTokens = null;
    else {
      const m = parseNum(f.maxTokens);
      if (m !== null) patch.maxTokens = Math.round(m);
    }
    patch.vision = f.vision === "auto" ? null : f.vision === "yes";
    patch.thinkingLevel = f.thinkingLevel;
    return ipc.writeConfig("llm", patch).catch((e) => setError(errMsg(e)));
  }, []);

  const scheduleLlmSave = useCallback(() => {
    window.clearTimeout(llmSaveTimer.current);
    llmSaveTimer.current = window.setTimeout(flushLlmSave, 500);
  }, [flushLlmSave]);

  const updateLlmForm = useCallback(
    (patch: Partial<LlmForm>) => {
      if (!llmFormRef.current) return;
      const next = { ...llmFormRef.current, ...patch };
      llmFormRef.current = next;
      setLlmForm(next);
      scheduleLlmSave();
    },
    [scheduleLlmSave],
  );

  const reloadLlmConfig = useCallback(() => {
    void ipc
      .readConfig("llm")
      .then((v) => {
        llmRawRef.current = v as Record<string, unknown>;
        const next = toLlmForm(v as LlmConfig);
        const focused = llmFocusedRef.current;
        if (focused && llmFormRef.current) {
          (next as unknown as Record<string, unknown>)[focused] = llmFormRef.current[focused];
        }
        // A new model id re-arms the context-window auto-fill; edits of the
        // field itself (same model) keep the "user edited" flag.
        if (lastModelIdRef.current !== null && lastModelIdRef.current !== next.model) {
          ctxUserEditedRef.current = false;
        }
        lastModelIdRef.current = next.model;
        llmFormRef.current = next;
        setLlmForm(next);
      })
      .catch((e) => setError(errMsg(e)));
  }, []);

  const loadVision = useCallback(() => {
    void ipc
      .getVisionStatus()
      .then(setVision)
      .catch(() => setVision(null));
  }, []);

  // detect_vision reads llm.json from disk, so pending form edits must land
  // first (same ordering as runLlmAction).
  const probeVision = () => {
    setVisionBusy(true);
    flushLlmSave()
      .then(() => ipc.detectVision())
      .then(setVision)
      .catch((e) => setError(errMsg(e)))
      .finally(() => setVisionBusy(false));
  };

  // Same read-modify-write pattern for app.json, shared by the "Агент" and
  // "Прочее" tabs.
  const flushAppSave = useCallback(() => {
    window.clearTimeout(appSaveTimer.current);
    const raw = appRawRef.current;
    const f = appFormRef.current;
    if (!raw || !f) return;
    const patch: Record<string, unknown> = { ...raw };
    // autostart is owned by set_autostart; keep its on-disk value untouched.
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
    void ipc.writeConfig("app", patch).catch((e) => setError(errMsg(e)));
  }, []);

  const scheduleAppSave = useCallback(() => {
    window.clearTimeout(appSaveTimer.current);
    appSaveTimer.current = window.setTimeout(flushAppSave, 500);
  }, [flushAppSave]);

  const updateAppForm = useCallback(
    (patch: Partial<AppForm>) => {
      if (!appFormRef.current) return;
      const next = { ...appFormRef.current, ...patch };
      appFormRef.current = next;
      setAppForm(next);
      scheduleAppSave();
    },
    [scheduleAppSave],
  );

  const reloadAppConfig = useCallback(() => {
    void ipc
      .readConfig("app")
      .then((v) => {
        appRawRef.current = v as Record<string, unknown>;
        const next = toAppForm(v as Partial<AppConfig>);
        const focused = appFocusedRef.current;
        if (focused && appFormRef.current) {
          (next as unknown as Record<string, unknown>)[focused] = appFormRef.current[focused];
        }
        appFormRef.current = next;
        setAppForm(next);
      })
      .catch((e) => setError(errMsg(e)));
  }, []);

  // test_connection/list_models read llm.json from disk, so the pending form
  // must be written first and the write must land before the command runs.
  const runLlmAction = useCallback((action: "test" | "models") => {
    setLlmBusy(action);
    flushLlmSave()
      .then(() =>
        action === "test"
          ? ipc.testConnection().then(setLlmTest)
          : ipc
              .listModels()
              .then((ms) => {
                setLlmModels(ms.map((m) => m.id));
                setLlmModelsError(null);
                modelsAutoLoadedRef.current = true;
              })
              .catch(() => {
                setLlmModels(null);
                setLlmModelsError(
                  "Endpoint не вернул список моделей — впиши название вручную (уточни у провайдера)",
                );
              }),
      )
      .catch((e) => {
        if (action === "test") setLlmTest({ ok: false, message: errMsg(e), modelCount: 0 });
      })
      .finally(() => setLlmBusy(null));
  }, [flushLlmSave]);

  // Auto-load the model list once, debounced, after endpoint+key are filled
  // in and nothing is loaded yet; a failure leaves the manual retry button.
  useEffect(() => {
    const f = llmForm;
    if (!f || modelsAutoLoadedRef.current) return;
    if (!f.baseUrl.trim() || !f.apiKey.trim()) return;
    if (llmModels !== null || llmBusy !== null) return;
    window.clearTimeout(modelsAutoTimer.current);
    modelsAutoTimer.current = window.setTimeout(() => {
      if (!modelsAutoLoadedRef.current) runLlmAction("models");
    }, 800);
    return () => window.clearTimeout(modelsAutoTimer.current);
  }, [llmForm?.baseUrl, llmForm?.apiKey, llmModels, llmBusy, runLlmAction]);

  // Auto-fill the context window from a size token in the model id
  // ("…-128k", "…-1m") while the field is still empty or on the default.
  useEffect(() => {
    const f = llmForm;
    if (!f || llmFocusedRef.current === "contextWindow") return;
    const hit = detectContextToken(f.model);
    if (!hit || ctxUserEditedRef.current) return;
    const cur = parseNum(f.contextWindow);
    if (cur !== null && cur !== DEFAULT_CONTEXT_WINDOW) return;
    if (cur !== null && Math.round(cur) === hit.tokens) return;
    setCtxAutoFilled(hit);
    updateLlmForm({ contextWindow: String(hit.tokens) });
  }, [llmForm, updateLlmForm]);

  const loadSkills = useCallback(() => {
    void ipc
      .skillsList()
      .then(setSkills)
      .catch((e) => setSkillsError(errMsg(e)));
  }, []);

  const loadMcp = useCallback(() => {
    void ipc
      .mcpStatus()
      .then(setMcp)
      .catch((e) => setMcpError(errMsg(e)));
  }, []);

  const loadBackupStatus = useCallback(() => {
    void ipc
      .backupStatus()
      .then(setBackupStatus)
      .catch(() => setBackupStatus(null));
  }, []);

  const loadTrash = useCallback(() => {
    void ipc
      .trashList()
      .then((rows) => {
        setTrash(rows);
        setTrashError(null);
      })
      .catch((e) => setTrashError(errMsg(e)));
  }, []);

  const loadProjects = useCallback(() => {
    void ipc
      .listProjects()
      .then(setProjects)
      .catch(() => setProjects([]));
  }, []);

  useEffect(() => {
    void ipc.getWorkingDir().then(setWorkingDir).catch(() => setWorkingDir(null));
    void ipc.configDirPath().then(setConfigDir).catch(() => setConfigDir(""));
    reloadConfig();
    reloadLlmConfig();
    loadVision();
    reloadAppConfig();
    void ipc.embedStatus().then(setEmbed).catch(() => setEmbed(null));
    void ipc.backupsDir().then(setBackups).catch(() => setBackups(""));
    void ipc.getAutostart().then(setAutostart).catch(() => setAutostart(null));
    loadSkills();
    loadMcp();
    loadBackupStatus();
    loadTrash();
    loadProjects();

    let cancelled = false;
    const unlisteners: (() => void)[] = [];
    const track = (p: Promise<() => void>) =>
      p.then((u) => (cancelled ? u() : unlisteners.push(u)));
    track(
      ipc.onConfigChanged(({ which }) => {
        if (which === "indexing") reloadConfig();
        else if (which === "llm") {
          reloadLlmConfig();
          loadVision();
        } else if (which === "app") {
          reloadAppConfig();
          loadBackupStatus();
        }
      }),
    );
    track(ipc.onTrashChanged(() => loadTrash()));
    track(
      ipc.onIndexChanged(({ stats }) =>
        setReindexNote(
          `Индекс обновлён: файлов ${stats.filesIndexed}, убрано ${stats.filesRemoved}`,
        ),
      ),
    );
    track(ipc.onSkillsChanged(() => loadSkills()));
    track(ipc.onProjectsChanged(() => loadProjects()));
    return () => {
      cancelled = true;
      unlisteners.forEach((u) => u());
      window.clearTimeout(saveTimer.current);
      flushSave();
      window.clearTimeout(llmSaveTimer.current);
      void flushLlmSave();
      window.clearTimeout(appSaveTimer.current);
      flushAppSave();
    };
  }, [
    reloadConfig,
    reloadLlmConfig,
    loadVision,
    reloadAppConfig,
    flushSave,
    flushLlmSave,
    flushAppSave,
    loadSkills,
    loadMcp,
    loadBackupStatus,
    loadTrash,
    loadProjects,
  ]);

  const reindex = () => {
    setReindexBusy(true);
    setReindexNote("Переиндексация…");
    void ipc
      .reindexAll()
      .then((results) => {
        const indexed = results.reduce((s, r) => s + r.stats.filesIndexed, 0);
        const seen = results.reduce((s, r) => s + r.stats.filesSeen, 0);
        const errors = results.reduce((s, r) => s + r.stats.errors.length, 0);
        setReindexNote(
          `Готово: индексировано ${indexed} из ${seen} файлов${errors ? `, ошибок: ${errors}` : ""}`,
        );
      })
      .catch((e) => setError(errMsg(e)))
      .finally(() => setReindexBusy(false));
  };

  const clearTmp = () => {
    setTmpBusy(true);
    setTmpNote(null);
    void ipc
      .clearTmp()
      .then((s) =>
        setTmpNote(
          `Удалено файлов: ${s.removedFiles} · освобождено ${fmtMb(s.freedBytes)} МБ`,
        ),
      )
      .catch((e) => setError(errMsg(e)))
      .finally(() => setTmpBusy(false));
  };

  const runBackup = () => {
    setBackupBusy(true);
    setBackupNote(null);
    void ipc
      .createBackup()
      .then((b) => {
        setBackupNote(
          `Готово: ${b.path} · файлов ${b.entries} · ${fmtMb(b.bytes)} МБ`,
        );
        loadBackupStatus();
      })
      .catch((e) => setError(errMsg(e)))
      .finally(() => setBackupBusy(false));
  };

  // Import a folder / .zip / Obsidian vault. folder and archive go into the
  // selected target (project needs a chosen project); Obsidian always lands in
  // the Notes vault. The picker is a directory chooser except for the archive.
  const runImport = async (kind: "folder" | "archive" | "obsidian") => {
    setImportBusy(true);
    setImportNote(null);
    setImportError(null);
    try {
      const picked = await open(
        kind === "archive"
          ? {
              multiple: false,
              title: "Импорт архива .zip",
              filters: [{ name: "Zip-архив", extensions: ["zip"] }],
            }
          : {
              directory: true,
              title: kind === "obsidian" ? "Импорт Obsidian vault" : "Импорт папки",
            },
      );
      if (typeof picked !== "string" || !picked) return;
      if (kind !== "obsidian" && importTarget === "project" && importProjectId == null) {
        setImportError("Выбери проект для импорта");
        return;
      }
      const s =
        kind === "obsidian"
          ? await ipc.importObsidian(picked)
          : kind === "folder"
            ? await ipc.importFolder(importTarget, importProjectId, picked)
            : await ipc.importArchive(importTarget, importProjectId, picked);
      const skipped = s.skipped > 0 ? ` · пропущено папок: ${s.skipped}` : "";
      setImportNote(`Импортировано файлов: ${s.importedFiles}${skipped} → ${s.dest}`);
    } catch (e) {
      setImportError(errMsg(e));
    } finally {
      setImportBusy(false);
    }
  };

  // Export a project or the whole Notes vault to a .zip. The save dialog
  // suggests "<name>-<timestamp>.zip"; the user picks the destination folder.
  const doExport = async (kind: "project" | "notes") => {
    setExportBusy(true);
    setExportNote(null);
    setExportError(null);
    try {
      if (kind === "project" && exportProjectId == null) {
        setExportError("Выбери проект для экспорта");
        return;
      }
      const proj = kind === "project" ? projects.find((p) => p.id === exportProjectId) : null;
      const suggested = `${kind === "project" ? (proj?.name ?? "project") : "notes"}-${exportStamp()}.zip`;
      const dest = await save({
        title: kind === "project" ? "Экспорт проекта в .zip" : "Экспорт заметок в .zip",
        defaultPath: suggested,
        filters: [{ name: "Zip-архив", extensions: ["zip"] }],
      });
      if (typeof dest !== "string" || !dest) return;
      const r =
        kind === "project"
          ? await ipc.exportProject(exportProjectId as number, dest)
          : await ipc.exportNotes(dest);
      setExportNote(`Готово: ${r.path} · файлов ${r.files}`);
    } catch (e) {
      setExportError(errMsg(e));
    } finally {
      setExportBusy(false);
    }
  };

  // Trash restore/purge emit trash-changed, which reloads the list; the busy id
  // only guards the button while the call is in flight.
  const restoreTrash = (id: number) => {
    setTrashBusyId(id);
    setTrashError(null);
    void ipc
      .trashRestore(id)
      .catch((e) => setTrashError(errMsg(e)))
      .finally(() => setTrashBusyId(null));
  };

  const purgeTrash = (id: number, name: string) => {
    if (!window.confirm(`Удалить «${name}» навсегда? Это действие необратимо.`)) return;
    setTrashBusyId(id);
    setTrashError(null);
    void ipc
      .trashPurge(id)
      .catch((e) => setTrashError(errMsg(e)))
      .finally(() => setTrashBusyId(null));
  };

  const toggleAutostart = (enabled: boolean) => {
    setAutostartBusy(true);
    void ipc
      .setAutostart(enabled)
      .then(setAutostart)
      .catch((e) => setError(errMsg(e)))
      .finally(() => setAutostartBusy(false));
  };

  // skills-changed refreshes the list, so the toggle needs no manual reload.
  const toggleSkill = (name: string, enabled: boolean) => {
    setSkillsError(null);
    void ipc.skillsSetEnabled(name, enabled).catch((e) => setSkillsError(errMsg(e)));
  };

  const doImport = async (source: "folder" | "zip" | "git") => {
    setSkillsBusy(true);
    setSkillsError(null);
    setImportResults(null);
    try {
      let target = "";
      if (source === "git") {
        target = gitUrl.trim();
        if (!target) {
          setSkillsError("Укажи git-адрес репозитория со скиллами");
          return;
        }
      } else {
        const picked = await open(
          source === "folder"
            ? { directory: true, title: "Импорт скиллов из папки" }
            : {
                multiple: false,
                title: "Импорт скиллов из .zip",
                filters: [{ name: "Zip-архив", extensions: ["zip"] }],
              },
        );
        if (typeof picked !== "string" || !picked) return;
        target = picked;
      }
      setImportResults(await ipc.skillsImport(source, target, overwrite));
      if (source === "git") setGitUrl("");
    } catch (e) {
      setSkillsError(errMsg(e));
    } finally {
      setSkillsBusy(false);
    }
  };

  const installBuiltin = () => {
    setSkillsBusy(true);
    setSkillsError(null);
    setImportResults(null);
    void ipc
      .skillsInstallBuiltin()
      .then((names) => setImportResults(names.map((name) => ({ name, ok: true, error: null }))))
      .catch((e) => setSkillsError(errMsg(e)))
      .finally(() => setSkillsBusy(false));
  };

  // skills_create emits skills-changed, so the list refreshes on its own.
  const createSkill = (e: FormEvent) => {
    e.preventDefault();
    setSkillFormError(null);
    setSkillNote(null);
    const name = skillForm.name.trim();
    if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(name) || name.length > 64) {
      setSkillFormError("Имя: строчные латинские буквы, цифры и дефис, не длиннее 64 символов");
      return;
    }
    const description = skillForm.description.trim();
    if (!description) {
      setSkillFormError("Описание — одна строка, обязательно");
      return;
    }
    setSkillCreateBusy(true);
    void ipc
      .skillsCreate(
        name,
        description,
        skillForm.kind,
        skillForm.body.trim() || undefined,
        skillForm.overwrite,
      )
      .then(() => {
        setSkillForm({ ...emptySkillForm });
        setSkillNote(`Скилл «${name}» создан`);
      })
      .catch((err) => setSkillFormError(errMsg(err)))
      .finally(() => setSkillCreateBusy(false));
  };

  const toggleMcp = async (name: string, enabled: boolean) => {
    setMcpBusy(true);
    setMcpError(null);
    try {
      setMcp(await patchMcpServers((servers) => servers.map((s) => (s.name === name ? { ...s, enabled } : s))));
    } catch (e) {
      setMcpError(errMsg(e));
    } finally {
      setMcpBusy(false);
    }
  };

  const reloadMcp = () => {
    setMcpBusy(true);
    setMcpError(null);
    void ipc
      .mcpReload()
      .then(setMcp)
      .catch((e) => setMcpError(errMsg(e)))
      .finally(() => setMcpBusy(false));
  };

  const saveMcpServer = async (e: FormEvent) => {
    e.preventDefault();
    setMcpFormError(null);
    const name = mcpForm.name.trim();
    if (!name) {
      setMcpFormError("Укажи имя сервера");
      return;
    }
    if (!/^[A-Za-z0-9][A-Za-z0-9_-]*$/.test(name)) {
      setMcpFormError("Имя: латиница, цифры, дефис или подчёркивание, без пробелов");
      return;
    }
    if (mcpEditing === null && mcp?.some((s) => s.name === name)) {
      setMcpFormError("Сервер с таким именем уже есть");
      return;
    }
    let transport: Record<string, unknown>;
    if (mcpForm.transport === "stdio") {
      const command = mcpForm.command.trim();
      if (!command) {
        setMcpFormError("Укажи команду — npx, node, python…");
        return;
      }
      const env = parseKeyLines(mcpForm.env, false);
      if (env === null) {
        setMcpFormError("Переменные окружения: каждая строка в формате KEY=VALUE");
        return;
      }
      transport = {
        type: "stdio",
        command,
        args: mcpForm.args.trim().split(/\s+/).filter(Boolean),
      };
      if (Object.keys(env).length) transport.env = env;
    } else {
      const url = mcpForm.url.trim();
      if (!url) {
        setMcpFormError("Укажи URL сервера");
        return;
      }
      if (!/^https?:\/\//i.test(url)) {
        setMcpFormError("URL должен начинаться с http:// или https://");
        return;
      }
      const headers = parseKeyLines(mcpForm.headers, true);
      if (headers === null) {
        setMcpFormError("Заголовки: строки вида Authorization: Bearer … или KEY=VALUE");
        return;
      }
      transport = { type: mcpForm.transport, url };
      if (Object.keys(headers).length) transport.headers = headers;
    }
    setMcpBusy(true);
    try {
      const editing = mcpEditing;
      setMcp(
        await patchMcpServers((servers) =>
          editing === null
            ? [...servers, { name, enabled: true, transport }]
            : servers.map((s) => (s.name === editing ? { ...s, name, transport } : s)),
        ),
      );
      setMcpForm({ ...emptyMcpForm });
      setMcpEditing(null);
    } catch (err) {
      setMcpFormError(errMsg(err));
    } finally {
      setMcpBusy(false);
    }
  };

  // Pre-fill the form from the raw mcp.json entry (status rows carry no
  // command/url), so hand-edited servers can be fixed from the UI.
  const startMcpEdit = (name: string) => {
    setMcpError(null);
    setMcpFormError(null);
    void ipc
      .readConfig("mcp")
      .then((raw) => {
        const servers = Array.isArray((raw as Record<string, unknown>).servers)
          ? ((raw as Record<string, unknown>).servers as Record<string, unknown>[])
          : [];
        const entry = servers.find((s) => s.name === name);
        if (!entry) {
          setMcpFormError("Сервер не найден в mcp.json — нажми «Перезагрузить»");
          return;
        }
        const t = (entry.transport ?? {}) as Record<string, unknown>;
        const kind: McpTransportKind =
          t.type === "http" || t.type === "sse" ? t.type : "stdio";
        const mapLines = (v: unknown, joiner: string) =>
          v && typeof v === "object"
            ? Object.entries(v as Record<string, unknown>)
                .map(([k, val]) => `${k}${joiner}${String(val)}`)
                .join("\n")
            : "";
        setMcpEditing(name);
        setMcpForm({
          name,
          transport: kind,
          command: String(t.command ?? ""),
          args: Array.isArray(t.args) ? t.args.map((a) => String(a)).join("\n") : "",
          env: mapLines(t.env, "="),
          url: String(t.url ?? ""),
          headers: mapLines(t.headers, ": "),
        });
      })
      .catch((e) => setMcpFormError(errMsg(e)));
  };

  const cancelMcpEdit = () => {
    setMcpEditing(null);
    setMcpForm({ ...emptyMcpForm });
    setMcpFormError(null);
  };

  const deleteMcpServer = async (name: string) => {
    if (!window.confirm(`Удалить сервер «${name}» из mcp.json?`)) return;
    setMcpBusy(true);
    setMcpError(null);
    try {
      setMcp(await patchMcpServers((servers) => servers.filter((s) => s.name !== name)));
      if (mcpEditing === name) cancelMcpEdit();
    } catch (e) {
      setMcpError(errMsg(e));
    } finally {
      setMcpBusy(false);
    }
  };

  const mcpArgsPreview = mcpForm.args.trim().split(/\s+/).filter(Boolean);

  const focusField = (name: keyof Form) => () => {
    focusedRef.current = name;
  };  const blurField = () => {
    focusedRef.current = null;
    flushSave();
  };

  const focusLlmField = (name: keyof LlmForm) => () => {
    llmFocusedRef.current = name;
  };
  const blurLlmField = () => {
    llmFocusedRef.current = null;
    void flushLlmSave();
  };

  const focusAppField = (name: keyof AppForm) => () => {
    appFocusedRef.current = name;
  };
  const blurAppField = () => {
    appFocusedRef.current = null;
    flushAppSave();
  };

  return (
    <div
      className="modal-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal settings-modal">
        <nav className="settings-nav">
          <div className="settings-title">Настройки</div>
          <button
            className={tab === "llm" ? "settings-tab active" : "settings-tab"}
            onClick={() => setTab("llm")}
          >
            Модель
          </button>
          <button
            className={tab === "agent" ? "settings-tab active" : "settings-tab"}
            onClick={() => setTab("agent")}
          >
            Агент
          </button>
          <button
            className={tab === "skills" ? "settings-tab active" : "settings-tab"}
            onClick={() => setTab("skills")}
          >
            Скиллы
          </button>
          <button
            className={tab === "mcp" ? "settings-tab active" : "settings-tab"}
            onClick={() => setTab("mcp")}
          >
            MCP
          </button>
          <button
            className={tab === "other" ? "settings-tab active" : "settings-tab"}
            onClick={() => setTab("other")}
          >
            Прочее
          </button>
        </nav>
        <div className="settings-body">
          <button className="icon-btn small settings-close" title="Закрыть" onClick={onClose}>
            <svg
              viewBox="0 0 24 24"
              width="15"
              height="15"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
            >
              <path d="M18 6 6 18M6 6l12 12" />
            </svg>
          </button>
          {tab === "llm" && (
            <div className="settings-content">
              <Card
                title="Подключение"
                desc="Endpoint и ключ OpenAI-совместимого провайдера. Изменения сохраняются автоматически."
              >
                <Field label="Endpoint" wide hint="Базовый адрес провайдера модели, как у OpenAI.">
                  <input
                    type="text"
                    value={llmForm?.baseUrl ?? ""}
                    onChange={(e) => updateLlmForm({ baseUrl: e.target.value })}
                    onFocus={focusLlmField("baseUrl")}
                    onBlur={blurLlmField}
                    placeholder="https://api.example.com/v1"
                  />
                </Field>
                <Field label="API-ключ" wide hint="Хранится в llm.json в открытом виде (см. ТЗ 3.4).">
                  <div className="llm-key-row">
                    <input
                      type={revealKey ? "text" : "password"}
                      value={llmForm?.apiKey ?? ""}
                      onChange={(e) => updateLlmForm({ apiKey: e.target.value })}
                      onFocus={focusLlmField("apiKey")}
                      onBlur={blurLlmField}
                      autoComplete="off"
                      spellCheck={false}
                    />
                    <button className="ghost-btn small" onClick={() => setRevealKey((v) => !v)}>
                      {revealKey ? "Скрыть" : "Показать"}
                    </button>
                  </div>
                </Field>
                <Field
                  label="Модель"
                  wide
                  hint="Список подтягивается с endpoint’а; можно вписать любое название вручную."
                  error={llmModelsError}
                >
                  <div className="model-row">
                    <ModelCombobox
                      value={llmForm?.model ?? ""}
                      models={llmModels}
                      onChange={(model) => updateLlmForm({ model })}
                      onFocus={focusLlmField("model")}
                      onBlur={blurLlmField}
                    />
                    <button
                      className="ghost-btn small"
                      disabled={llmBusy !== null}
                      onClick={() => runLlmAction("models")}
                    >
                      {llmBusy === "models" && <span className="spinner"></span>}
                      {llmBusy === "models" ? "Загрузка…" : "Загрузить модели"}
                    </button>
                  </div>
                </Field>
                <div className="set-actions">
                  <button
                    className="ghost-btn"
                    disabled={llmBusy !== null}
                    onClick={() => runLlmAction("test")}
                  >
                    {llmBusy === "test" && <span className="spinner"></span>}
                    {llmBusy === "test" ? "Проверка…" : "Проверить подключение"}
                  </button>
                  {llmTest && (
                    <div className={llmTest.ok ? "llm-test ok" : "llm-test err"}>
                      <span className="mark">{llmTest.ok ? "✓" : "✕"}</span>
                      {llmTest.message}
                    </div>
                  )}
                </div>
              </Card>
              <Card title="Генерация" desc="Параметры запросов к модели.">
                <Field
                  label="Окно контекста, токенов"
                  hint={
                    ctxAutoFilled
                      ? `Заполнено по названию модели (${ctxAutoFilled.token}) — можно исправить руками.`
                      : detectContextToken(llmForm?.model ?? "")
                        ? "В поле уже вписано значение — оно не меняется автоматически."
                        : "Размер не угадывается по названию модели — заполни вручную."
                  }
                >
                  <input
                    className="narrow"
                    type="text"
                    inputMode="numeric"
                    value={llmForm?.contextWindow ?? ""}
                    onChange={(e) => {
                      ctxUserEditedRef.current = true;
                      setCtxAutoFilled(null);
                      updateLlmForm({ contextWindow: e.target.value });
                    }}
                    onFocus={focusLlmField("contextWindow")}
                    onBlur={blurLlmField}
                  />
                </Field>
                <Field
                  label="Temperature"
                  hint="Фиксирована для каждого агента и не настраивается: проекты и сессии харнеса — 0.2, заметки — 0.4. Поле в llm.json игнорируется."
                />
                <Field
                  label="Максимум токенов в ответе"
                  hint="Пусто — без ограничения со стороны приложения."
                >
                  <input
                    className="narrow"
                    type="text"
                    inputMode="numeric"
                    value={llmForm?.maxTokens ?? ""}
                    onChange={(e) => updateLlmForm({ maxTokens: e.target.value })}
                    onFocus={focusLlmField("maxTokens")}
                    onBlur={blurLlmField}
                  />
                </Field>
                <Field
                  label="Уровень размышлений"
                  hint="Сколько модель думает перед ответом (reasoning_effort). Ход мыслей показывается свёрнутым блоком «Размышления» в чате и отправляется обратно модели в следующем запросе. Если провайдер не принимает reasoning_effort, ошибка придёт в чат."
                >
                  <select
                    value={llmForm?.thinkingLevel ?? "medium"}
                    onChange={(e) => {
                      updateLlmForm({
                        thinkingLevel: e.target.value as "off" | "low" | "medium" | "high",
                      });
                      void flushLlmSave();
                    }}
                  >
                    <option value="off">Выключено</option>
                    <option value="low">Низкий</option>
                    <option value="medium">Средний</option>
                    <option value="high">Высокий</option>
                  </select>
                </Field>
              </Card>
              <Card
                title="Распознавание изображений (vision)"
                desc={
                  <>
                    Статус: {visionLabel(vision)}
                    {vision?.overridden ? " (задано вручную)" : ""}
                    {vision?.reason ? ` — ${vision.reason}` : ""}
                  </>
                }
              >
                <Field
                  label="Отправлять изображения модели"
                  hint="Если провайдер не поддерживает vision, изображения из чата не будут отправлены модели."
                >
                  <select
                    value={llmForm?.vision ?? "auto"}
                    onChange={(e) => {
                      updateLlmForm({ vision: e.target.value as "auto" | "yes" | "no" });
                      void flushLlmSave();
                    }}
                  >
                    <option value="auto">Авто (определять)</option>
                    <option value="yes">Да</option>
                    <option value="no">Нет</option>
                  </select>
                </Field>
                <div className="set-actions">
                  <button className="ghost-btn" disabled={visionBusy} onClick={probeVision}>
                    {visionBusy ? "Проверка…" : "Проверить"}
                  </button>
                </div>
              </Card>
              {error && <div className="set-error">{error}</div>}
            </div>
          )}
          {tab === "agent" && (
            <div className="settings-content">
              <Card title="Автопилот">
                <Field
                  label="Полный автопилот (все разрешения без подтверждений)"
                  hint="Опасно: агент выполняет любые действия без подтверждений. По умолчанию выключено."
                >
                  <label className="switch">
                    <input
                      type="checkbox"
                      checked={appForm?.autopilot ?? false}
                      onChange={(e) => updateAppForm({ autopilot: e.target.checked })}
                    />
                    <span className="track"></span>
                  </label>
                </Field>
              </Card>
              <Card title="Индексация" desc="Ограничения на файлы, попадающие в поиск по проектам.">
                <Field
                  label="Максимальный размер файла, МБ"
                  hint="Файлы больше этого размера в индекс не попадают. По умолчанию 10 МБ."
                >
                  <input
                    className="narrow"
                    type="text"
                    inputMode="decimal"
                    value={form?.maxMb ?? ""}
                    onChange={(e) => updateForm({ maxMb: e.target.value })}
                    onFocus={focusField("maxMb")}
                    onBlur={blurField}
                  />
                </Field>
                <Field
                  label="Лимит извлечённого текста, символов"
                  hint="Из одного большого файла берётся не больше этого куска текста. По умолчанию 1 000 000 символов."
                >
                  <input
                    className="narrow"
                    type="text"
                    inputMode="numeric"
                    value={form?.maxChars ?? ""}
                    onChange={(e) => updateForm({ maxChars: e.target.value })}
                    onFocus={focusField("maxChars")}
                    onBlur={blurField}
                  />
                </Field>
                <Field
                  label="Исключать из индекса (через запятую)"
                  wide
                  hint="Папки, которые поиск по проекту никогда не увидит."
                >
                  <input
                    type="text"
                    value={form?.excludeDirs ?? ""}
                    onChange={(e) => updateForm({ excludeDirs: e.target.value })}
                    onFocus={focusField("excludeDirs")}
                    onBlur={blurField}
                  />
                </Field>
              </Card>
              <Card title="Переиндексация" desc="Полное перестроение индекса по всем проектам.">
                <div className="set-actions">
                  <button className="ghost-btn" disabled={reindexBusy} onClick={reindex}>
                    {reindexBusy ? "Переиндексация…" : "Переиндексировать"}
                  </button>
                </div>
                {reindexNote && <div className="set-note">{reindexNote}</div>}
              </Card>
              <Card
                title="Семантический поиск"
                desc="Семантический поиск включён всегда: поиск по словам и по смыслу работают вместе (локальная модель EmbeddingGemma 2, векторы 768). Отключить или подстроить его нельзя."
              >
                {embed && (
                  <div className="set-note">
                    Статус модели:{" "}
                    {embed.available
                      ? "скачана, активна"
                      : embed.modelPresent
                        ? "файлы на месте, но модель недоступна"
                        : "не скачана — сейчас поиск только по словам"}
                  </div>
                )}
                {embed?.reason && <div className="set-note">{embed.reason}</div>}
              </Card>
              {error && <div className="set-error">{error}</div>}
            </div>
          )}
          {tab === "skills" && (
            <div className="settings-content">
              <Card
                title="Импорт скиллов"
                desc="Загрузка скиллов из папки, архива или git-репозитория."
              >
                <Field
                  label="Перезаписывать при импорте"
                  hint="Заменяет уже установленный пользовательский скилл с тем же именем."
                >
                  <label className="switch">
                    <input
                      type="checkbox"
                      checked={overwrite}
                      onChange={(e) => setOverwrite(e.target.checked)}
                    />
                    <span className="track"></span>
                  </label>
                </Field>
                <Field
                  label="Git-адрес репозитория со скиллами"
                  wide
                  hint="Нужен для кнопки «Импорт из git»; требуется системный git."
                >
                  <input
                    type="text"
                    value={gitUrl}
                    onChange={(e) => setGitUrl(e.target.value)}
                    placeholder="https://github.com/example/skills"
                    spellCheck={false}
                  />
                </Field>
                <div className="set-actions">
                  <button
                    className="ghost-btn"
                    disabled={skillsBusy}
                    onClick={() => void doImport("folder")}
                  >
                    Импорт из папки
                  </button>
                  <button
                    className="ghost-btn"
                    disabled={skillsBusy}
                    onClick={() => void doImport("zip")}
                  >
                    Импорт из .zip
                  </button>
                  <button
                    className="ghost-btn"
                    disabled={skillsBusy}
                    onClick={() => void doImport("git")}
                  >
                    Импорт из git
                  </button>
                </div>
                {importResults && (
                  <div className="skill-results">
                    {importResults.map((r, i) => (
                      <div key={`${r.name}-${i}`} className={r.ok ? "set-note" : "set-error"}>
                        {r.name}: {r.ok ? "импортирован" : r.error}
                      </div>
                    ))}
                  </div>
                )}
              </Card>
              <Card
                title="Встроенные скиллы"
                desc="Копирует набор встроенных скиллов приложения в папку настроек."
              >
                <div className="set-actions">
                  <button className="ghost-btn" disabled={skillsBusy} onClick={installBuiltin}>
                    Установить встроенные
                  </button>
                </div>
              </Card>
              <Card title="Создать скилл" desc="Новый пользовательский скилл появится в списке ниже.">
                <form className="set-form" onSubmit={createSkill}>
                  <Field
                    label="Имя (slug)"
                    hint="Строчные латинские буквы, цифры и дефис; по этому имени агент и вызывает скилл."
                  >
                    <input
                      type="text"
                      value={skillForm.name}
                      onChange={(e) => setSkillForm({ ...skillForm, name: e.target.value })}
                      placeholder="my-skill"
                      spellCheck={false}
                    />
                  </Field>
                  <Field label="Описание (одна строка)">
                    <input
                      type="text"
                      value={skillForm.description}
                      onChange={(e) => setSkillForm({ ...skillForm, description: e.target.value })}
                      placeholder="Что делает этот скилл"
                    />
                  </Field>
                  <Field label="Тип">
                    <select
                      value={skillForm.kind}
                      onChange={(e) =>
                        setSkillForm({ ...skillForm, kind: e.target.value as SkillKindOption })
                      }
                    >
                      <option value="instruction">instruction (инструкция)</option>
                      <option value="script">script (скрипт)</option>
                      <option value="extension">extension (расширение)</option>
                    </select>
                  </Field>
                  <Field label="Тело скилла (markdown, необязательно)" wide>
                    <textarea
                      rows={6}
                      value={skillForm.body}
                      onChange={(e) => setSkillForm({ ...skillForm, body: e.target.value })}
                      placeholder="Полные инструкции для агента…"
                      spellCheck={false}
                    />
                  </Field>
                  <Field label="перезаписать, если существует">
                    <label className="switch">
                      <input
                        type="checkbox"
                        checked={skillForm.overwrite}
                        onChange={(e) =>
                          setSkillForm({ ...skillForm, overwrite: e.target.checked })
                        }
                      />
                      <span className="track"></span>
                    </label>
                  </Field>
                  <div className="set-actions">
                    <button className="primary-btn small" type="submit" disabled={skillCreateBusy}>
                      {skillCreateBusy ? "Создание…" : "Создать"}
                    </button>
                  </div>
                  {skillFormError && <div className="set-error">{skillFormError}</div>}
                  {skillNote && <div className="set-note">{skillNote}</div>}
                </form>
              </Card>
              <Card title="Список скиллов">
                {skills === null ? (
                  <div className="set-empty">Загрузка…</div>
                ) : skills.length === 0 ? (
                  <div className="set-empty">Скиллов нет. Установи встроенные или импортируй свои.</div>
                ) : (
                  <div className="set-list">
                    {skills.map((s, i) =>
                      s.error ? (
                        <div key={`${s.name}-${i}`} className="skill-item skill-item-error">
                          <div className="info">
                            <div className="t">{s.name || "(без имени)"}</div>
                            <div className="c">{s.error}</div>
                          </div>
                          {s.builtin && <span className="badge">встроенный</span>}
                        </div>
                      ) : (
                        <div key={`${s.name}-${i}`} className="skill-item">
                          <div className="info">
                            <div className="t">{s.name}</div>
                            <div className="c">{s.description}</div>
                          </div>
                          {s.builtin && <span className="badge">встроенный</span>}
                          <label className="switch">
                            <input
                              type="checkbox"
                              checked={s.enabled}
                              onChange={(e) => toggleSkill(s.name, e.target.checked)}
                            />
                            <span className="track"></span>
                          </label>
                        </div>
                      ),
                    )}
                  </div>
                )}
              </Card>
              {skillsError && <div className="set-error">{skillsError}</div>}
            </div>
          )}
          {tab === "mcp" && (
            <div className="settings-content">
              <details className="set-card">
                <summary>Как подключить MCP-сервер</summary>
                <div className="set-card-body">
                  <div>
                    Локально (stdio): команда <code>npx</code>, аргументы — по одному на строку:{" "}
                    <code>-y @modelcontextprotocol/server-filesystem /path</code>
                  </div>
                  <div>
                    Удалённо (http): URL <code>https://mcp.example.com/mcp</code> и заголовок{" "}
                    <code>Authorization: Bearer …</code>
                  </div>
                  <div>
                    Инструменты попадают в каталог как <code>mcp_сервер__инструмент</code> и по
                    умолчанию требуют подтверждения перед вызовом.
                  </div>
                </div>
              </details>
              <Card title="Серверы" desc="Перечитывает mcp.json и перезапускает серверы.">
                <div className="set-actions">
                  <button className="ghost-btn" disabled={mcpBusy} onClick={reloadMcp}>
                    Перезагрузить
                  </button>
                </div>
                {mcp === null ? (
                  <div className="set-empty">Загрузка…</div>
                ) : mcp.length === 0 ? (
                  <div className="set-empty">Серверов нет. Добавь первый ниже.</div>
                ) : (
                  <div className="set-list">
                    {mcp.map((s, i) => (
                      <div key={`${s.name}-${i}`} className="mcp-item">
                        <div className="info">
                          <div className="t">{s.name}</div>
                          <div className="c">
                            {s.status === "running" ? `инструментов: ${s.toolCount}` : ""}
                            {s.error ? (s.status === "running" ? ` · ${s.error}` : s.error) : ""}
                          </div>
                        </div>
                        <span className="badge">{s.transport}</span>
                        <span
                          className={`badge st-${s.status === "running" ? "active" : s.status}`}
                        >
                          {s.status === "running"
                            ? "запущен"
                            : s.status === "error"
                              ? "ошибка"
                              : "выключен"}
                        </span>
                        <span className="mcp-actions">
                          <button className="ghost-btn small" onClick={() => startMcpEdit(s.name)}>
                            Изменить
                          </button>
                          <button
                            className="ghost-btn small"
                            onClick={() => void deleteMcpServer(s.name)}
                          >
                            Удалить
                          </button>
                        </span>
                        <label className="switch">
                          <input
                            type="checkbox"
                            checked={s.enabled}
                            disabled={mcpBusy}
                            onChange={(e) => void toggleMcp(s.name, e.target.checked)}
                          />
                          <span className="track"></span>
                        </label>
                      </div>
                    ))}
                  </div>
                )}
                {mcpError && <div className="set-error">{mcpError}</div>}
              </Card>
              <Card
                title={mcpEditing === null ? "Добавить сервер" : `Изменить сервер «${mcpEditing}»`}
                desc="Сервер сохраняется в mcp.json и сразу перезапускается."
              >
                <form className="set-form" onSubmit={(e) => void saveMcpServer(e)}>
                  <Field
                    label="Имя сервера"
                    hint="Латиница, цифры, дефис и подчёркивание — по имени сервера называются его инструменты."
                  >
                    <input
                      type="text"
                      value={mcpForm.name}
                      onChange={(e) => setMcpForm({ ...mcpForm, name: e.target.value })}
                      placeholder="example-mcp"
                      spellCheck={false}
                    />
                  </Field>
                  <Field label="Тип транспорта">
                    <select
                      value={mcpForm.transport}
                      onChange={(e) =>
                        setMcpForm({ ...mcpForm, transport: e.target.value as McpTransportKind })
                      }
                    >
                      <option value="stdio">stdio (локальный процесс)</option>
                      <option value="http">http (по адресу)</option>
                      <option value="sse">sse (по адресу, legacy)</option>
                    </select>
                  </Field>
                  {mcpForm.transport === "stdio" ? (
                    <>
                      <Field label="Команда" hint="Программа, запускающая сервер (npx, node, python).">
                        <input
                          type="text"
                          value={mcpForm.command}
                          onChange={(e) => setMcpForm({ ...mcpForm, command: e.target.value })}
                          placeholder="npx"
                          spellCheck={false}
                        />
                      </Field>
                      <Field label="Аргументы (по одному на строку или через пробел)" wide>
                        <textarea
                          rows={2}
                          value={mcpForm.args}
                          onChange={(e) => setMcpForm({ ...mcpForm, args: e.target.value })}
                          placeholder={"-y\n@example/mcp-server\n/path"}
                          spellCheck={false}
                        />
                        {mcpArgsPreview.length > 0 && (
                          <div className="mcp-chips">
                            {mcpArgsPreview.map((a, i) => (
                              <span key={i} className="mcp-chip">
                                {a}
                              </span>
                            ))}
                          </div>
                        )}
                      </Field>
                      <Field label="Переменные окружения (необязательно)" wide hint="По одной строке KEY=VALUE.">
                        <textarea
                          rows={2}
                          value={mcpForm.env}
                          onChange={(e) => setMcpForm({ ...mcpForm, env: e.target.value })}
                          placeholder="API_KEY=secret — по одной строке KEY=VALUE"
                          spellCheck={false}
                        />
                      </Field>
                    </>
                  ) : (
                    <>
                      <Field
                        label="URL сервера"
                        wide
                        hint="Адрес http/sse-сервера; адреса облачных метаданных запрещены."
                      >
                        <input
                          type="text"
                          value={mcpForm.url}
                          onChange={(e) => setMcpForm({ ...mcpForm, url: e.target.value })}
                          placeholder="https://mcp.example.com/mcp"
                          spellCheck={false}
                        />
                      </Field>
                      <Field
                        label="Заголовки (необязательно)"
                        wide
                        hint="Строки вида Authorization: Bearer … или KEY=VALUE."
                      >
                        <textarea
                          rows={2}
                          value={mcpForm.headers}
                          onChange={(e) => setMcpForm({ ...mcpForm, headers: e.target.value })}
                          placeholder={"Authorization: Bearer …\nX-Api-Key=…"}
                          spellCheck={false}
                        />
                      </Field>
                    </>
                  )}
                  <div className="set-actions">
                    <button className="primary-btn small" type="submit" disabled={mcpBusy}>
                      {mcpEditing === null ? "Добавить" : "Сохранить"}
                    </button>
                    {mcpEditing !== null && (
                      <button className="ghost-btn small" type="button" onClick={cancelMcpEdit}>
                        Отмена
                      </button>
                    )}
                  </div>
                  {mcpFormError && <div className="set-error">{mcpFormError}</div>}
                </form>
              </Card>
            </div>
          )}
          {tab === "other" && (
            <div className="settings-content">
              <Card title="Расположение" desc="Папки, где приложение хранит данные и настройки.">
                <Field label="Рабочая директория" wide>
                  <div className="path-value">{workingDir ?? "—"}</div>
                </Field>
                <Field label="Директория настроек" wide>
                  <div className="path-value">{configDir || "—"}</div>
                </Field>
              </Card>
              <Card
                title="Резервные копии"
                desc={
                  backupStatus === null
                    ? "Статус резервных копий недоступен."
                    : `Последняя: ${
                        backupStatus.lastBackupAt ? fmtRfc(backupStatus.lastBackupAt) : "ещё не делалась"
                      } · на диске: ${backupStatus.count} · ${
                        backupStatus.intervalDays === 0
                          ? "автовыключено"
                          : backupStatus.due
                            ? "пора сделать сейчас"
                            : `авто: каждые ${backupStatus.intervalDays} дн.`
                      }`
                }
              >
                <Field
                  label="Интервал автокопий, дней"
                  hint="Каждые N дней при запуске создаётся копия. 0 — выключено."
                >
                  <input
                    className="narrow"
                    type="text"
                    inputMode="numeric"
                    value={appForm?.backupIntervalDays ?? ""}
                    onChange={(e) => updateAppForm({ backupIntervalDays: e.target.value })}
                    onFocus={focusAppField("backupIntervalDays")}
                    onBlur={blurAppField}
                  />
                </Field>
                <Field label="Хранить копий" hint="Сколько последних копий остаётся на диске. 0 — одна.">
                  <input
                    className="narrow"
                    type="text"
                    inputMode="numeric"
                    value={appForm?.backupKeep ?? ""}
                    onChange={(e) => updateAppForm({ backupKeep: e.target.value })}
                    onFocus={focusAppField("backupKeep")}
                    onBlur={blurAppField}
                  />
                </Field>
                <div className="set-actions">
                  <button className="ghost-btn" disabled={backupBusy} onClick={runBackup}>
                    {backupBusy ? "Создание…" : "Резервная копия"}
                  </button>
                </div>
                {backupNote && <div className="set-note">{backupNote}</div>}
                {backups && <div className="set-note">Резервные копии по умолчанию: {backups}</div>}
              </Card>
              <Card
                title="Корзина"
                desc="Удалённые файлы, папки и задачи попадают в корзину и хранятся 30 дней."
              >
                {trash === null ? (
                  <div className="set-empty">Загрузка…</div>
                ) : trash.length === 0 ? (
                  <div className="set-empty">Корзина пуста.</div>
                ) : (
                  <div className="set-list">
                    {trash.map((t) => (
                      <div key={t.id} className="skill-item">
                        <div className="info">
                          <div className="t">{t.name || "(без имени)"}</div>
                          <div className="c">{t.originalPath}</div>
                          <div className="c">удалено {fmtLocal(t.deletedAt)}</div>
                        </div>
                        <span className="badge">{t.kind}</span>
                        <span className="badge st-disabled">{t.scope}</span>
                        <span className="item-actions">
                          <button
                            className="ghost-btn small"
                            disabled={trashBusyId !== null}
                            onClick={() => restoreTrash(t.id)}
                          >
                            {trashBusyId === t.id ? "…" : "Восстановить"}
                          </button>
                          <button
                            className="ghost-btn small danger"
                            disabled={trashBusyId !== null}
                            onClick={() => purgeTrash(t.id, t.name)}
                          >
                            Удалить навсегда
                          </button>
                        </span>
                      </div>
                    ))}
                  </div>
                )}
                {trashError && <div className="set-error">{trashError}</div>}
              </Card>
              <Card
                title="Импорт и экспорт"
                desc="Импорт папок, архивов и Obsidian vault; экспорт проекта или заметок в .zip."
              >
                <Field label="Куда импортировать">
                  <select
                    value={importTarget}
                    onChange={(e) => setImportTarget(e.target.value as "project" | "notes")}
                  >
                    <option value="project">Проект</option>
                    <option value="notes">Заметки</option>
                  </select>
                </Field>
                {importTarget === "project" && (
                  <Field label="Проект для импорта">
                    <select
                      value={importProjectId ?? ""}
                      onChange={(e) =>
                        setImportProjectId(e.target.value ? Number(e.target.value) : null)
                      }
                    >
                      <option value="">— выбери проект —</option>
                      {projects.map((p) => (
                        <option key={p.id} value={p.id}>
                          {p.name}
                        </option>
                      ))}
                    </select>
                  </Field>
                )}
                <div className="set-actions">
                  <button
                    className="ghost-btn"
                    disabled={importBusy}
                    onClick={() => void runImport("folder")}
                  >
                    Папка
                  </button>
                  <button
                    className="ghost-btn"
                    disabled={importBusy}
                    onClick={() => void runImport("archive")}
                  >
                    Архив .zip
                  </button>
                  <button
                    className="ghost-btn"
                    disabled={importBusy}
                    onClick={() => void runImport("obsidian")}
                  >
                    Obsidian vault
                  </button>
                </div>
                <div className="set-note">
                  Папка и архив идут в выбранную цель, Obsidian vault — в заметки без служебных
                  папок (.obsidian, .trash, .git). При совпадении имени добавляется суффикс « copy»,
                  файлы не перезаписываются.
                </div>
                {importNote && <div className="set-note">{importNote}</div>}
                {importError && <div className="set-error">{importError}</div>}
                <Field label="Проект для экспорта">
                  <select
                    value={exportProjectId ?? ""}
                    onChange={(e) =>
                      setExportProjectId(e.target.value ? Number(e.target.value) : null)
                    }
                  >
                    <option value="">— выбери проект —</option>
                    {projects.map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name}
                      </option>
                    ))}
                  </select>
                </Field>
                <div className="set-actions">
                  <button
                    className="ghost-btn"
                    disabled={exportBusy}
                    onClick={() => void doExport("project")}
                  >
                    Экспорт проекта
                  </button>
                  <button
                    className="ghost-btn"
                    disabled={exportBusy}
                    onClick={() => void doExport("notes")}
                  >
                    Экспорт заметок
                  </button>
                </div>
                <div className="set-note">
                  Проект и заметки упаковываются в .zip: диалог предлагает имя, папку выбираешь сам.
                </div>
                {exportNote && <div className="set-note">{exportNote}</div>}
                {exportError && <div className="set-error">{exportError}</div>}
              </Card>
              <Card title="Обслуживание" desc="Автоочистка при запуске и запуск с системой.">
                <Field
                  label="Автоудаление сессий харнеса, дней"
                  hint="Сессии без активности дольше этого срока удаляются при запуске. 0 — отключено."
                >
                  <input
                    className="narrow"
                    type="text"
                    inputMode="numeric"
                    value={appForm?.sessionTtlDays ?? ""}
                    onChange={(e) => updateAppForm({ sessionTtlDays: e.target.value })}
                    onFocus={focusAppField("sessionTtlDays")}
                    onBlur={blurAppField}
                  />
                </Field>
                <Field
                  label="Автоочистка Harness/tmp, дней"
                  hint="Файлы в Harness/tmp старше этого срока стираются при запуске. 0 — не стирать."
                >
                  <input
                    className="narrow"
                    type="text"
                    inputMode="numeric"
                    value={appForm?.tmpRetentionDays ?? ""}
                    onChange={(e) => updateAppForm({ tmpRetentionDays: e.target.value })}
                    onFocus={focusAppField("tmpRetentionDays")}
                    onBlur={blurAppField}
                  />
                </Field>
                <Field
                  label="Хранение журнала запусков, дней"
                  hint="Записи журнала старше этого срока удаляются при запуске. 0 — не удалять."
                >
                  <input
                    className="narrow"
                    type="text"
                    inputMode="numeric"
                    value={appForm?.logRetentionDays ?? ""}
                    onChange={(e) => updateAppForm({ logRetentionDays: e.target.value })}
                    onFocus={focusAppField("logRetentionDays")}
                    onBlur={blurAppField}
                  />
                </Field>
                <Field
                  label="Запускаться с системой"
                  hint="macOS LaunchAgent, чтобы расписания работали без открытого окна. По умолчанию выключено."
                >
                  <label className="switch">
                    <input
                      type="checkbox"
                      checked={autostart ?? false}
                      disabled={autostart === null || autostartBusy}
                      onChange={(e) => toggleAutostart(e.target.checked)}
                    />
                    <span className="track"></span>
                  </label>
                </Field>
                <div className="set-actions">
                  <button className="ghost-btn" disabled={tmpBusy} onClick={clearTmp}>
                    {tmpBusy ? "Очистка…" : "Очистить Harness/tmp"}
                  </button>
                </div>
                {tmpNote && <div className="set-note">{tmpNote}</div>}
              </Card>
              {error && <div className="set-error">{error}</div>}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

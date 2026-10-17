import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import * as ipc from "../api/ipc";
import type {
  AppConfig,
  ConnectionTest,
  EmbedStatus,
  ImportResult,
  IndexingConfig,
  LlmConfig,
  McpServerStatus,
  SkillInfo,
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
}

function toAppForm(cfg: Partial<AppConfig>): AppForm {
  return {
    autopilot: cfg.autopilot ?? false,
    sessionTtlDays: String(cfg.sessionTtlDays ?? 30),
    tmpRetentionDays: String(cfg.tmpRetentionDays ?? 7),
    logRetentionDays: String(cfg.logRetentionDays ?? 14),
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
  const runLlmAction = (action: "test" | "models") => {
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
              })
              .catch(() => {
                setLlmModels(null);
                setLlmModelsError(
                  "Endpoint не вернул список моделей — впиши название вручную, узнай у провайдера",
                );
              }),
      )
      .catch((e) => {
        if (action === "test") setLlmTest({ ok: false, message: errMsg(e), modelCount: 0 });
      })
      .finally(() => setLlmBusy(null));
  };

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
        } else if (which === "app") reloadAppConfig();
      }),
    );
    track(
      ipc.onIndexChanged(({ stats }) =>
        setReindexNote(
          `Индекс обновлён: файлов ${stats.filesIndexed}, убрано ${stats.filesRemoved}`,
        ),
      ),
    );
    track(ipc.onSkillsChanged(() => loadSkills()));
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
      .then((b) =>
        setBackupNote(
          `Готово: ${b.path} · файлов ${b.entries} · ${fmtMb(b.bytes)} МБ`,
        ),
      )
      .catch((e) => setError(errMsg(e)))
      .finally(() => setBackupBusy(false));
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
            <>
              <div className="set-row">
                <label>Endpoint (OpenAI-совместимый)</label>
                <input
                  type="text"
                  value={llmForm?.baseUrl ?? ""}
                  onChange={(e) => updateLlmForm({ baseUrl: e.target.value })}
                  onFocus={focusLlmField("baseUrl")}
                  onBlur={blurLlmField}
                  placeholder="https://api.example.com/v1"
                />
                <div className="set-hint">Базовый адрес провайдера модели, как у OpenAI.</div>
              </div>
              <div className="set-row">
                <label>API-ключ</label>
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
                <div className="set-hint">Хранится в llm.json в открытом виде (см. ТЗ 3.4).</div>
              </div>
              {llmModels && llmModels.length > 0 && (
                <div className="set-row">
                  <label>Модель из списка endpoint’а</label>
                  <select
                    value={llmModels.includes(llmForm?.model ?? "") ? (llmForm?.model ?? "") : ""}
                    onChange={(e) => updateLlmForm({ model: e.target.value })}
                  >
                    {!llmModels.includes(llmForm?.model ?? "") && (
                      <option value="">— выбрать модель —</option>
                    )}
                    {llmModels.map((id) => (
                      <option key={id} value={id}>
                        {id}
                      </option>
                    ))}
                  </select>
                </div>
              )}
              <div className="set-row">
                <label>Модель</label>
                <input
                  type="text"
                  value={llmForm?.model ?? ""}
                  onChange={(e) => updateLlmForm({ model: e.target.value })}
                  onFocus={focusLlmField("model")}
                  onBlur={blurLlmField}
                />
                {llmModelsError && <div className="set-error">{llmModelsError}</div>}
              </div>
              <div className="set-row">
                <label>Окно контекста, токенов</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="numeric"
                  value={llmForm?.contextWindow ?? ""}
                  onChange={(e) => updateLlmForm({ contextWindow: e.target.value })}
                  onFocus={focusLlmField("contextWindow")}
                  onBlur={blurLlmField}
                />
                <div className="set-hint">
                  Для выбранной модели подтягивается автоматически, если модель его сообщает;
                  иначе — дефолт 100 000, можно исправить руками.
                </div>
              </div>
              <div className="set-row">
                <label>Temperature</label>
                <div className="set-hint">
                  Фиксирована для каждого агента и не настраивается: проекты и сессии харнеса —
                  0.2, заметки — 0.4. Поле в llm.json игнорируется.
                </div>
              </div>
              <div className="set-row">
                <label>Максимум токенов в ответе (необязательно)</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="numeric"
                  value={llmForm?.maxTokens ?? ""}
                  onChange={(e) => updateLlmForm({ maxTokens: e.target.value })}
                  onFocus={focusLlmField("maxTokens")}
                  onBlur={blurLlmField}
                />
                <div className="set-hint">Пусто — без ограничения со стороны приложения.</div>
              </div>
              <div className="set-row">
                <label>Уровень размышлений</label>
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
                <div className="set-hint">
                  Сколько модель думает перед ответом (reasoning_effort). Ход мыслей показывается
                  свёрнутым блоком «Размышления» в чате и отправляется обратно модели в следующем
                  запросе. Если провайдер не принимает reasoning_effort, ошибка придёт в чат.
                </div>
              </div>
              <div className="set-row">
                <label>Распознавание изображений (vision)</label>
                <div className="set-hint">
                  Статус: {visionLabel(vision)}
                  {vision?.overridden ? " (задано вручную)" : ""}
                </div>
                {vision?.reason && <div className="set-hint">{vision.reason}</div>}
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
                <div className="set-hint">
                  Если провайдер не поддерживает vision, изображения из чата не будут отправлены модели.
                </div>
                <button className="ghost-btn" disabled={visionBusy} onClick={probeVision}>
                  {visionBusy ? "Проверка…" : "Проверить"}
                </button>
              </div>
              <div className="llm-actions">
                <button
                  className="ghost-btn"
                  disabled={llmBusy !== null}
                  onClick={() => runLlmAction("test")}
                >
                  {llmBusy === "test" ? "Проверка…" : "Проверить подключение"}
                </button>
                <button
                  className="ghost-btn"
                  disabled={llmBusy !== null}
                  onClick={() => runLlmAction("models")}
                >
                  {llmBusy === "models" ? "Загрузка…" : "Загрузить модели"}
                </button>
              </div>
              {llmTest && (
                <div className={llmTest.ok ? "set-hint" : "set-error"}>{llmTest.message}</div>
              )}
              {error && <div className="set-error">{error}</div>}
            </>
          )}
          {tab === "agent" && (
            <>
              <div className="set-row switch-row">
                <label>Полный автопилот (все разрешения без подтверждений)</label>
                <label className="switch">
                  <input
                    type="checkbox"
                    checked={appForm?.autopilot ?? false}
                    onChange={(e) => updateAppForm({ autopilot: e.target.checked })}
                  />
                  <span className="track"></span>
                </label>
              </div>
              <div className="set-hint">
                Опасно: агент выполняет любые действия без подтверждений. По умолчанию выключено.
              </div>
              <div className="schedule-group-title">Индексация</div>
              <div className="set-row">
                <label>Максимальный размер файла, МБ</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="decimal"
                  value={form?.maxMb ?? ""}
                  onChange={(e) => updateForm({ maxMb: e.target.value })}
                  onFocus={focusField("maxMb")}
                  onBlur={blurField}
                />
                <div className="set-hint">
                  Файлы больше этого размера в индекс не попадают. По умолчанию 10 МБ.
                </div>
              </div>
              <div className="set-row">
                <label>Лимит извлечённого текста, символов</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="numeric"
                  value={form?.maxChars ?? ""}
                  onChange={(e) => updateForm({ maxChars: e.target.value })}
                  onFocus={focusField("maxChars")}
                  onBlur={blurField}
                />
                <div className="set-hint">
                  Из одного большого файла берётся не больше этого куска текста. По умолчанию
                  1 000 000 символов.
                </div>
              </div>
              <div className="set-row">
                <label>Исключать из индекса (через запятую)</label>
                <input
                  type="text"
                  value={form?.excludeDirs ?? ""}
                  onChange={(e) => updateForm({ excludeDirs: e.target.value })}
                  onFocus={focusField("excludeDirs")}
                  onBlur={blurField}
                />
                <div className="set-hint">Папки, которые поиск по проекту никогда не увидит.</div>
              </div>
              <button className="ghost-btn" disabled={reindexBusy} onClick={reindex}>
                {reindexBusy ? "Переиндексация…" : "Переиндексировать"}
              </button>
              {reindexNote && <div className="set-hint">{reindexNote}</div>}

              <div className="schedule-group-title">Семантический поиск</div>
              <div className="set-hint">
                Семантический поиск включён всегда: поиск по словам и по смыслу работают вместе
                (локальная модель EmbeddingGemma 2, векторы 768). Отключить или подстроить его
                нельзя.
              </div>
              {embed && (
                <div className="set-hint">
                  Статус модели:{" "}
                  {embed.available
                    ? "скачана, активна"
                    : embed.modelPresent
                      ? "файлы на месте, но модель недоступна"
                      : "не скачана — сейчас поиск только по словам"}
                </div>
              )}
              {embed?.reason && <div className="set-hint">{embed.reason}</div>}
              {error && <div className="set-error">{error}</div>}
            </>
          )}
          {tab === "skills" && (
            <>
              <div className="set-row switch-row">
                <label>Перезаписывать при импорте</label>
                <label className="switch">
                  <input
                    type="checkbox"
                    checked={overwrite}
                    onChange={(e) => setOverwrite(e.target.checked)}
                  />
                  <span className="track"></span>
                </label>
              </div>
              <div className="set-hint">
                Заменяет уже установленный пользовательский скилл с тем же именем.
              </div>
              <div className="llm-actions">
                <button className="ghost-btn" disabled={skillsBusy} onClick={() => void doImport("folder")}>
                  Импорт из папки
                </button>
                <button className="ghost-btn" disabled={skillsBusy} onClick={() => void doImport("zip")}>
                  Импорт из .zip
                </button>
                <button className="ghost-btn" disabled={skillsBusy} onClick={() => void doImport("git")}>
                  Импорт из git
                </button>
              </div>
              <div className="set-row">
                <label>Git-адрес репозитория со скиллами</label>
                <input
                  type="text"
                  value={gitUrl}
                  onChange={(e) => setGitUrl(e.target.value)}
                  placeholder="https://github.com/example/skills"
                  spellCheck={false}
                />
                <div className="set-hint">Нужен для кнопки «Импорт из git»; требуется системный git.</div>
              </div>
              <button className="ghost-btn" disabled={skillsBusy} onClick={installBuiltin}>
                Установить встроенные
              </button>
              <div className="set-hint">Копирует набор встроенных скиллов приложения в папку настроек.</div>
              <details className="mcp-guide">
                <summary>Создать скилл</summary>
                <form className="schedule-form" onSubmit={createSkill}>
                  <div className="set-row">
                    <label>Имя (slug)</label>
                    <input
                      type="text"
                      value={skillForm.name}
                      onChange={(e) => setSkillForm({ ...skillForm, name: e.target.value })}
                      placeholder="my-skill"
                      spellCheck={false}
                    />
                    <div className="set-hint">
                      Строчные латинские буквы, цифры и дефис; по этому имени агент и вызывает скилл.
                    </div>
                  </div>
                  <div className="set-row">
                    <label>Описание (одна строка)</label>
                    <input
                      type="text"
                      value={skillForm.description}
                      onChange={(e) => setSkillForm({ ...skillForm, description: e.target.value })}
                      placeholder="Что делает этот скилл"
                    />
                  </div>
                  <div className="set-row">
                    <label>Тип</label>
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
                  </div>
                  <div className="set-row">
                    <label>Тело скилла (markdown, необязательно)</label>
                    <textarea
                      rows={6}
                      value={skillForm.body}
                      onChange={(e) => setSkillForm({ ...skillForm, body: e.target.value })}
                      placeholder="Полные инструкции для агента…"
                      spellCheck={false}
                    />
                  </div>
                  <div className="set-row switch-row">
                    <label>перезаписать, если существует</label>
                    <label className="switch">
                      <input
                        type="checkbox"
                        checked={skillForm.overwrite}
                        onChange={(e) => setSkillForm({ ...skillForm, overwrite: e.target.checked })}
                      />
                      <span className="track"></span>
                    </label>
                  </div>
                  <div className="form-actions">
                    <button className="primary-btn small" type="submit" disabled={skillCreateBusy}>
                      {skillCreateBusy ? "Создание…" : "Создать"}
                    </button>
                  </div>
                  {skillFormError && <div className="set-error">{skillFormError}</div>}
                  {skillNote && <div className="set-hint">{skillNote}</div>}
                </form>
              </details>
              {importResults && (
                <div className="skill-results">
                  {importResults.map((r, i) => (
                    <div key={`${r.name}-${i}`} className={r.ok ? "set-hint" : "set-error"}>
                      {r.name}: {r.ok ? "импортирован" : r.error}
                    </div>
                  ))}
                </div>
              )}
              <div className="schedule-group-title">Список скиллов</div>
              {skills === null ? (
                <div className="set-hint">Загрузка…</div>
              ) : skills.length === 0 ? (
                <div className="set-hint">Скиллов нет. Установи встроенные или импортируй свои.</div>
              ) : (
                <div className="skill-list">
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
              {skillsError && <div className="set-error">{skillsError}</div>}
            </>
          )}
          {tab === "mcp" && (
            <>
              <details className="mcp-guide">
                <summary>Как подключить MCP-сервер</summary>
                <div className="mcp-guide-body">
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
              <div className="llm-actions">
                <button className="ghost-btn" disabled={mcpBusy} onClick={reloadMcp}>
                  Перезагрузить
                </button>
              </div>
              <div className="set-hint">Перечитывает mcp.json и перезапускает серверы.</div>
              <div className="schedule-group-title">Серверы</div>
              {mcp === null ? (
                <div className="set-hint">Загрузка…</div>
              ) : mcp.length === 0 ? (
                <div className="set-hint">Серверов нет. Добавь первый ниже.</div>
              ) : (
                <div className="mcp-list">
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
                      <span className={`badge st-${s.status === "running" ? "active" : s.status}`}>
                        {s.status === "running" ? "запущен" : s.status === "error" ? "ошибка" : "выключен"}
                      </span>
                      <span className="mcp-actions">
                        <button className="ghost-btn small" onClick={() => startMcpEdit(s.name)}>
                          Изменить
                        </button>
                        <button className="ghost-btn small" onClick={() => void deleteMcpServer(s.name)}>
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
              <div className="schedule-group-title">
                {mcpEditing === null ? "Добавить сервер" : `Изменить сервер «${mcpEditing}»`}
              </div>
              <form className="schedule-form" onSubmit={(e) => void saveMcpServer(e)}>
                <div className="set-row">
                  <label>Имя сервера</label>
                  <input
                    type="text"
                    value={mcpForm.name}
                    onChange={(e) => setMcpForm({ ...mcpForm, name: e.target.value })}
                    placeholder="example-mcp"
                    spellCheck={false}
                  />
                  <div className="set-hint">
                    Латиница, цифры, дефис и подчёркивание — по имени сервера называются его
                    инструменты.
                  </div>
                </div>
                <div className="set-row">
                  <label>Тип транспорта</label>
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
                </div>
                {mcpForm.transport === "stdio" ? (
                  <>
                    <div className="set-row">
                      <label>Команда</label>
                      <input
                        type="text"
                        value={mcpForm.command}
                        onChange={(e) => setMcpForm({ ...mcpForm, command: e.target.value })}
                        placeholder="npx"
                        spellCheck={false}
                      />
                      <div className="set-hint">Программа, запускающая сервер (npx, node, python).</div>
                    </div>
                    <div className="set-row">
                      <label>Аргументы (по одному на строку или через пробел)</label>
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
                    </div>
                    <div className="set-row">
                      <label>Переменные окружения (необязательно)</label>
                      <textarea
                        rows={2}
                        value={mcpForm.env}
                        onChange={(e) => setMcpForm({ ...mcpForm, env: e.target.value })}
                        placeholder="API_KEY=secret — по одной строке KEY=VALUE"
                        spellCheck={false}
                      />
                    </div>
                  </>
                ) : (
                  <>
                    <div className="set-row">
                      <label>URL сервера</label>
                      <input
                        type="text"
                        value={mcpForm.url}
                        onChange={(e) => setMcpForm({ ...mcpForm, url: e.target.value })}
                        placeholder="https://mcp.example.com/mcp"
                        spellCheck={false}
                      />
                      <div className="set-hint">
                        Адрес http/sse-сервера; адреса облачных метаданных запрещены.
                      </div>
                    </div>
                    <div className="set-row">
                      <label>Заголовки (необязательно)</label>
                      <textarea
                        rows={2}
                        value={mcpForm.headers}
                        onChange={(e) => setMcpForm({ ...mcpForm, headers: e.target.value })}
                        placeholder={"Authorization: Bearer …\nX-Api-Key=…"}
                        spellCheck={false}
                      />
                    </div>
                  </>
                )}
                <div className="form-actions">
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
            </>
          )}
          {tab === "other" && (
            <>
              <div className="set-row">
                <label>Рабочая директория</label>
                <div className="path-value">{workingDir ?? "—"}</div>
              </div>
              <div className="set-row">
                <label>Директория настроек</label>
                <div className="path-value">{configDir || "—"}</div>
              </div>
              <div className="set-row">
                <label>Автоудаление сессий харнеса, дней</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="numeric"
                  value={appForm?.sessionTtlDays ?? ""}
                  onChange={(e) => updateAppForm({ sessionTtlDays: e.target.value })}
                  onFocus={focusAppField("sessionTtlDays")}
                  onBlur={blurAppField}
                />
                <div className="set-hint">
                  Сессии без активности дольше этого срока удаляются при запуске. 0 — отключено.
                </div>
              </div>
              <div className="set-row">
                <label>Автоочистка Harness/tmp, дней</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="numeric"
                  value={appForm?.tmpRetentionDays ?? ""}
                  onChange={(e) => updateAppForm({ tmpRetentionDays: e.target.value })}
                  onFocus={focusAppField("tmpRetentionDays")}
                  onBlur={blurAppField}
                />
                <div className="set-hint">
                  Файлы в Harness/tmp старше этого срока стираются при запуске. 0 — не стирать.
                </div>
              </div>
              <div className="set-row">
                <label>Хранение журнала запусков, дней</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="numeric"
                  value={appForm?.logRetentionDays ?? ""}
                  onChange={(e) => updateAppForm({ logRetentionDays: e.target.value })}
                  onFocus={focusAppField("logRetentionDays")}
                  onBlur={blurAppField}
                />
                <div className="set-hint">
                  Записи журнала старше этого срока удаляются при запуске. 0 — не удалять.
                </div>
              </div>
              <div className="llm-actions">
                <button className="ghost-btn" disabled={tmpBusy} onClick={clearTmp}>
                  {tmpBusy ? "Очистка…" : "Очистить Harness/tmp"}
                </button>
                <button className="ghost-btn" disabled={backupBusy} onClick={runBackup}>
                  {backupBusy ? "Создание…" : "Резервная копия"}
                </button>
              </div>
              {tmpNote && <div className="set-hint">{tmpNote}</div>}
              {backupNote && <div className="set-hint">{backupNote}</div>}
              {backups && <div className="set-hint">Резервные копии по умолчанию: {backups}</div>}
              <div className="set-row switch-row">
                <label>Запускаться с системой</label>
                <label className="switch">
                  <input
                    type="checkbox"
                    checked={autostart ?? false}
                    disabled={autostart === null || autostartBusy}
                    onChange={(e) => toggleAutostart(e.target.checked)}
                  />
                  <span className="track"></span>
                </label>
              </div>
              <div className="set-hint">
                macOS LaunchAgent, чтобы расписания работали без открытого окна. По умолчанию
                выключено.
              </div>
              {error && <div className="set-error">{error}</div>}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

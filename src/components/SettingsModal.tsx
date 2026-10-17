import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import * as ipc from "../api/ipc";
import type {
  AppConfig,
  ConnectionTest,
  EmbedModelProgress,
  EmbedStatus,
  ImportResult,
  IndexingConfig,
  LlmConfig,
  McpServerStatus,
  MemoryDto,
  SkillInfo,
} from "../api/types";

const MB = 1024 * 1024;

// Form fields are kept as strings so mid-typing states ("20.", "") survive;
// parsing happens only at save time.
interface Form {
  maxMb: string;
  maxChars: string;
  excludeDirs: string;
  backend: string;
  ollamaModel: string;
  dim: string;
  nameWeight: string;
  enabled: boolean;
}

function toForm(cfg: IndexingConfig): Form {
  return {
    maxMb: String(Math.round((cfg.maxFileSizeBytes / MB) * 100) / 100),
    maxChars: String(cfg.maxTextChars),
    excludeDirs: cfg.excludeDirs.join(", "),
    backend: cfg.embedding.backend,
    ollamaModel: cfg.embedding.ollamaModel,
    dim: String(cfg.embedding.dim),
    nameWeight: String(cfg.embedding.nameWeight),
    enabled: cfg.embedding.enabled,
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
  temperature: string;
  maxTokens: string;
}

function toLlmForm(cfg: LlmConfig): LlmForm {
  return {
    baseUrl: cfg.baseUrl,
    apiKey: cfg.apiKey,
    model: cfg.model,
    contextWindow: String(cfg.contextWindow),
    temperature: cfg.temperature == null ? "" : String(cfg.temperature),
    maxTokens: cfg.maxTokens == null ? "" : String(cfg.maxTokens),
  };
}

function fmtMb(bytes: number): string {
  return (bytes / MB).toFixed(1);
}

// app.json fields edited by the "Агент" and "Прочее" tabs share one form so a
// single debounced writer owns the file.
interface AppForm {
  agentName: string;
  userAddress: string;
  autopilot: boolean;
  sessionTtlDays: string;
  tmpRetentionDays: string;
  logRetentionDays: string;
}

function toAppForm(cfg: Partial<AppConfig>): AppForm {
  return {
    agentName: cfg.agentName ?? "Агент",
    userAddress: cfg.userAddress ?? "на ты",
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

// Comma-separated tag input → trimmed non-empty list (same normalization the
// backend applies).
function parseTags(s: string): string[] {
  return s
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);
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
  const [tab, setTab] = useState<"llm" | "index" | "agent" | "skills" | "mcp" | "memory" | "other">("llm");
  const [workingDir, setWorkingDir] = useState<string | null>(null);
  const [configDir, setConfigDir] = useState("");
  const [form, setForm] = useState<Form | null>(null);
  const [embed, setEmbed] = useState<EmbedStatus | null>(null);
  const [embedBusy, setEmbedBusy] = useState(false);
  const [embedProgress, setEmbedProgress] = useState<EmbedModelProgress | null>(null);
  const [reindexBusy, setReindexBusy] = useState(false);
  const [reindexNote, setReindexNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [llmForm, setLlmForm] = useState<LlmForm | null>(null);
  const [llmModels, setLlmModels] = useState<string[] | null>(null);
  const [llmModelsError, setLlmModelsError] = useState<string | null>(null);
  const [llmTest, setLlmTest] = useState<ConnectionTest | null>(null);
  const [llmBusy, setLlmBusy] = useState<"test" | "models" | null>(null);
  const [revealKey, setRevealKey] = useState(false);
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

  const [mcp, setMcp] = useState<McpServerStatus[] | null>(null);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [mcpBusy, setMcpBusy] = useState(false);
  const [mcpForm, setMcpForm] = useState({
    name: "",
    transport: "stdio",
    command: "",
    args: "",
    url: "",
  });
  const [mcpFormError, setMcpFormError] = useState<string | null>(null);

  const [mem, setMem] = useState<MemoryDto[] | null>(null);
  const [memError, setMemError] = useState<string | null>(null);
  const [memQuery, setMemQuery] = useState("");
  const [memNew, setMemNew] = useState({ content: "", tags: "" });
  const [memEditingId, setMemEditingId] = useState<number | null>(null);
  const [memDraft, setMemDraft] = useState({ content: "", tags: "" });
  const [projectNames, setProjectNames] = useState<Record<number, string>>({});

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

  // Search query kept in a ref so loadMemory (and the memory-changed
  // listener) stay stable while typing.
  const memQueryRef = useRef("");

  const flushSave = useCallback(() => {
    window.clearTimeout(saveTimer.current);
    const raw = rawRef.current;
    const f = formRef.current;
    if (!raw || !f) return;
    const embRaw =
      raw.embedding && typeof raw.embedding === "object"
        ? (raw.embedding as Record<string, unknown>)
        : {};
    const emb: Record<string, unknown> = { ...embRaw };
    const dim = parseNum(f.dim);
    if (dim !== null) emb.dim = Math.round(dim);
    const nameWeight = parseNum(f.nameWeight);
    if (nameWeight !== null) emb.nameWeight = nameWeight;
    emb.backend = f.backend;
    emb.ollamaModel = f.ollamaModel;
    emb.enabled = f.enabled;
    const patch: Record<string, unknown> = { ...raw, embedding: emb };
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
    if (f.temperature.trim() === "") patch.temperature = null;
    else {
      const t = parseNum(f.temperature);
      if (t !== null) patch.temperature = t;
    }
    if (f.maxTokens.trim() === "") patch.maxTokens = null;
    else {
      const m = parseNum(f.maxTokens);
      if (m !== null) patch.maxTokens = Math.round(m);
    }
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
    patch.agentName = f.agentName;
    patch.userAddress = f.userAddress;
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

  const loadMemory = useCallback(() => {
    const q = memQueryRef.current.trim();
    const req = q ? ipc.memorySearch(q, 100) : ipc.memoryList();
    void req
      .then(setMem)
      .catch((e) => setMemError(errMsg(e)));
  }, []);

  useEffect(() => {
    void ipc.getWorkingDir().then(setWorkingDir).catch(() => setWorkingDir(null));
    void ipc.configDirPath().then(setConfigDir).catch(() => setConfigDir(""));
    reloadConfig();
    reloadLlmConfig();
    reloadAppConfig();
    void ipc.embedStatus().then(setEmbed).catch(() => setEmbed(null));
    void ipc.backupsDir().then(setBackups).catch(() => setBackups(""));
    void ipc.getAutostart().then(setAutostart).catch(() => setAutostart(null));
    loadSkills();
    loadMcp();
    loadMemory();
    void ipc
      .listProjects()
      .then((ps) => setProjectNames(Object.fromEntries(ps.map((p) => [p.id, p.name]))))
      .catch(() => setProjectNames({}));

    let cancelled = false;
    const unlisteners: (() => void)[] = [];
    const track = (p: Promise<() => void>) =>
      p.then((u) => (cancelled ? u() : unlisteners.push(u)));
    track(
      ipc.onConfigChanged(({ which }) => {
        if (which === "indexing") reloadConfig();
        else if (which === "llm") reloadLlmConfig();
        else if (which === "app") reloadAppConfig();
      }),
    );
    track(
      ipc.onIndexChanged(({ stats }) =>
        setReindexNote(
          `Индекс обновлён: файлов ${stats.filesIndexed}, убрано ${stats.filesRemoved}`,
        ),
      ),
    );
    track(ipc.onEmbedModelProgress((p) => setEmbedProgress(p)));
    track(ipc.onSkillsChanged(() => loadSkills()));
    track(ipc.onMemoryChanged(() => loadMemory()));
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
    reloadAppConfig,
    flushSave,
    flushLlmSave,
    flushAppSave,
    loadSkills,
    loadMcp,
    loadMemory,
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

  const downloadModel = () => {
    setEmbedBusy(true);
    setEmbedProgress(null);
    void ipc
      .downloadEmbedModel()
      .then(setEmbed)
      .catch((e) => setError(errMsg(e)))
      .finally(() => {
        setEmbedBusy(false);
        setEmbedProgress(null);
      });
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

  // memory-changed reloads the list, so add/update/delete need no manual refresh.
  const addMemory = (e: FormEvent) => {
    e.preventDefault();
    const content = memNew.content.trim();
    if (!content) {
      setMemError("Впиши текст записи");
      return;
    }
    setMemError(null);
    void ipc
      .memoryAdd(content, undefined, parseTags(memNew.tags))
      .then(() => setMemNew({ content: "", tags: "" }))
      .catch((err) => setMemError(errMsg(err)));
  };

  const startMemoryEdit = (m: MemoryDto) => {
    setMemError(null);
    setMemEditingId(m.id);
    setMemDraft({ content: m.content, tags: m.tags.join(", ") });
  };

  const saveMemoryEdit = (id: number) => {
    const content = memDraft.content.trim();
    if (!content) {
      setMemError("Запись не может быть пустой");
      return;
    }
    setMemError(null);
    void ipc
      .memoryUpdate(id, content, parseTags(memDraft.tags))
      .then(() => setMemEditingId(null))
      .catch((err) => setMemError(errMsg(err)));
  };

  const deleteMemory = (m: MemoryDto) => {
    if (!window.confirm("Удалить запись из памяти?")) return;
    setMemError(null);
    void ipc.memoryDelete(m.id).catch((err) => setMemError(errMsg(err)));
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

  const addMcpServer = async (e: FormEvent) => {
    e.preventDefault();
    setMcpFormError(null);
    const name = mcpForm.name.trim();
    if (!name) {
      setMcpFormError("Укажи имя сервера");
      return;
    }
    setMcpBusy(true);
    try {
      const transport =
        mcpForm.transport === "stdio"
          ? {
              type: "stdio",
              command: mcpForm.command.trim(),
              args: mcpForm.args.trim().split(/\s+/).filter(Boolean),
            }
          : { type: "http", url: mcpForm.url.trim() };
      setMcp(
        await patchMcpServers((servers) => [...servers, { name, enabled: true, transport }]),
      );
      setMcpForm({ name: "", transport: "stdio", command: "", args: "", url: "" });
    } catch (err) {
      setMcpFormError(errMsg(err));
    } finally {
      setMcpBusy(false);
    }
  };

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
            className={tab === "index" ? "settings-tab active" : "settings-tab"}
            onClick={() => setTab("index")}
          >
            Индексация
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
            className={tab === "memory" ? "settings-tab active" : "settings-tab"}
            onClick={() => setTab("memory")}
          >
            Память
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
                <label>Temperature (необязательно)</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="decimal"
                  value={llmForm?.temperature ?? ""}
                  onChange={(e) => updateLlmForm({ temperature: e.target.value })}
                  onFocus={focusLlmField("temperature")}
                  onBlur={blurLlmField}
                />
                <div className="set-hint">Пусто — значение по умолчанию провайдера.</div>
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
          {tab === "index" && (
            <>
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
                <div className="set-hint">Файлы больше этого размера в индекс не попадают.</div>
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
                <div className="set-hint">Из одного большого файла берётся не больше этого куска текста.</div>
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

              <div className="set-row switch-row">
                <label>Семантический поиск (эмбеддинги)</label>
                <label className="switch">
                  <input
                    type="checkbox"
                    checked={form?.enabled ?? false}
                    onChange={(e) => updateForm({ enabled: e.target.checked })}
                  />
                  <span className="track"></span>
                </label>
              </div>
              <div className="set-row">
                <label>Бэкенд эмбеддингов</label>
                <select
                  value={form?.backend ?? "noop"}
                  onChange={(e) => updateForm({ backend: e.target.value })}
                  onFocus={focusField("backend")}
                  onBlur={blurField}
                >
                  <option value="noop">noop (без эмбеддингов)</option>
                  <option value="ollama">ollama</option>
                  <option value="ort">ort (локальная модель)</option>
                </select>
              </div>
              {form?.backend === "ollama" && (
                <div className="set-row">
                  <label>Модель Ollama</label>
                  <input
                    type="text"
                    value={form?.ollamaModel ?? ""}
                    onChange={(e) => updateForm({ ollamaModel: e.target.value })}
                    onFocus={focusField("ollamaModel")}
                    onBlur={blurField}
                  />
                </div>
              )}
              <div className="set-row">
                <label>Размерность векторов (dim)</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="numeric"
                  value={form?.dim ?? ""}
                  onChange={(e) => updateForm({ dim: e.target.value })}
                  onFocus={focusField("dim")}
                  onBlur={blurField}
                />
              </div>
              <div className="set-row">
                <label>Вес совпадения по имени (nameWeight)</label>
                <input
                  className="narrow"
                  type="text"
                  inputMode="decimal"
                  value={form?.nameWeight ?? ""}
                  onChange={(e) => updateForm({ nameWeight: e.target.value })}
                  onFocus={focusField("nameWeight")}
                  onBlur={blurField}
                />
              </div>
              {embed && (
                <div className="set-hint">
                  Статус эмбеддингов: {embed.backend} ·{" "}
                  {embed.available ? "доступен" : "недоступен"}
                  {embed.backend === "ort" &&
                    ` · модель ${embed.modelPresent ? "скачана" : "не скачана"}`}{" "}
                  · dim {embed.dim}
                </div>
              )}
              {embed?.reason && <div className="set-hint">{embed.reason}</div>}
              {form?.backend === "ort" && (
                <button className="ghost-btn" disabled={embedBusy} onClick={downloadModel}>
                  {embedBusy ? "Скачивание…" : "Скачать модель"}
                </button>
              )}
              {embedProgress && (
                <div className="set-hint">
                  {embedProgress.file}: {fmtMb(embedProgress.done)} из {fmtMb(embedProgress.total)} МБ
                </div>
              )}
              {error && <div className="set-error">{error}</div>}
            </>
          )}
          {tab === "agent" && (
            <>
              <div className="set-row">
                <label>Имя агента</label>
                <input
                  type="text"
                  value={appForm?.agentName ?? ""}
                  onChange={(e) => updateAppForm({ agentName: e.target.value })}
                  onFocus={focusAppField("agentName")}
                  onBlur={blurAppField}
                />
                <div className="set-hint">Как агент подписывается в ответах; имя уходит в память.</div>
              </div>
              <div className="set-row">
                <label>Как обращаться к пользователю</label>
                <input
                  type="text"
                  value={appForm?.userAddress ?? ""}
                  onChange={(e) => updateAppForm({ userAddress: e.target.value })}
                  onFocus={focusAppField("userAddress")}
                  onBlur={blurAppField}
                />
                <div className="set-hint">Например «на ты» или «на Вы» — тон общения агента.</div>
              </div>
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
                      <span className={`badge st-${s.status === "running" ? "active" : s.status}`}>
                        {s.status === "running" ? "запущен" : s.status === "error" ? "ошибка" : "выключен"}
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
              <div className="schedule-group-title">Добавить сервер</div>
              <form className="schedule-form" onSubmit={(e) => void addMcpServer(e)}>
                <div className="set-row">
                  <label>Имя сервера</label>
                  <input
                    type="text"
                    value={mcpForm.name}
                    onChange={(e) => setMcpForm({ ...mcpForm, name: e.target.value })}
                    placeholder="example-mcp"
                    spellCheck={false}
                  />
                  <div className="set-hint">Латиница без пробелов — имя сервера в mcp.json.</div>
                </div>
                <div className="set-row">
                  <label>Тип транспорта</label>
                  <select
                    value={mcpForm.transport}
                    onChange={(e) => setMcpForm({ ...mcpForm, transport: e.target.value })}
                  >
                    <option value="stdio">stdio (локальный процесс)</option>
                    <option value="http">http (по адресу)</option>
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
                      <label>Аргументы (через пробел)</label>
                      <input
                        type="text"
                        value={mcpForm.args}
                        onChange={(e) => setMcpForm({ ...mcpForm, args: e.target.value })}
                        placeholder="-y @example/mcp-server"
                        spellCheck={false}
                      />
                      <div className="set-hint">Аргументы команды, разделённые пробелом.</div>
                    </div>
                  </>
                ) : (
                  <div className="set-row">
                    <label>URL сервера</label>
                    <input
                      type="text"
                      value={mcpForm.url}
                      onChange={(e) => setMcpForm({ ...mcpForm, url: e.target.value })}
                      placeholder="http://localhost:9000/mcp"
                      spellCheck={false}
                    />
                    <div className="set-hint">Адрес http/sse-сервера; транспорт http/sse пока не запускается.</div>
                  </div>
                )}
                <div className="form-actions">
                  <button className="primary-btn small" type="submit" disabled={mcpBusy}>
                    Добавить
                  </button>
                </div>
                {mcpFormError && <div className="set-error">{mcpFormError}</div>}
              </form>
            </>
          )}
          {tab === "memory" && (
            <>
              <div className="set-hint">
                Память едина для всех чатов — и проектов, и сессий харнеса. Агент читает и
                пополняет её сам через remember/recall, поэтому записи отсюда доступны ему в
                любом чате.
              </div>
              <div className="set-row">
                <label>Поиск по памяти</label>
                <input
                  type="text"
                  value={memQuery}
                  onChange={(e) => {
                    memQueryRef.current = e.target.value;
                    setMemQuery(e.target.value);
                    loadMemory();
                  }}
                  placeholder="ключевые слова"
                />
              </div>
              <div className="schedule-group-title">
                {memQuery.trim() ? "Найденные записи" : "Все записи"}
              </div>
              {mem === null ? (
                <div className="set-hint">Загрузка…</div>
              ) : mem.length === 0 ? (
                <div className="set-hint">
                  {memQuery.trim()
                    ? "Ничего не найдено."
                    : "Записей нет. Добавь ниже или попроси агента запомнить."}
                </div>
              ) : (
                <div className="memory-list">
                  {mem.map((m) =>
                    memEditingId === m.id ? (
                      <div key={m.id} className="memory-item memory-editing">
                        <textarea
                          rows={3}
                          value={memDraft.content}
                          onChange={(e) => setMemDraft({ ...memDraft, content: e.target.value })}
                        />
                        <input
                          type="text"
                          value={memDraft.tags}
                          placeholder="теги через запятую"
                          onChange={(e) => setMemDraft({ ...memDraft, tags: e.target.value })}
                        />
                        <div className="form-actions">
                          <button className="primary-btn small" onClick={() => saveMemoryEdit(m.id)}>
                            Сохранить
                          </button>
                          <button className="ghost-btn small" onClick={() => setMemEditingId(null)}>
                            Отмена
                          </button>
                        </div>
                      </div>
                    ) : (
                      <div key={m.id} className="memory-item">
                        <div className="content">{m.content}</div>
                        <div className="memory-meta">
                          {m.source?.startsWith("identity:") && (
                            <span className="badge">из настроек</span>
                          )}
                          {m.projectId != null && (
                            <span className="badge">
                              проект: {projectNames[m.projectId] ?? `#${m.projectId}`}
                            </span>
                          )}
                          {m.tags.map((t) => (
                            <span key={t} className="badge">
                              {t}
                            </span>
                          ))}
                          <span className="memory-actions">
                            <button className="ghost-btn small" onClick={() => startMemoryEdit(m)}>
                              Изменить
                            </button>
                            <button className="ghost-btn small" onClick={() => deleteMemory(m)}>
                              Удалить
                            </button>
                          </span>
                        </div>
                      </div>
                    ),
                  )}
                </div>
              )}
              {memError && <div className="set-error">{memError}</div>}
              <div className="schedule-group-title">Новая запись</div>
              <form className="schedule-form" onSubmit={(e) => addMemory(e)}>
                <textarea
                  rows={3}
                  value={memNew.content}
                  placeholder="что агент должен запомнить"
                  onChange={(e) => setMemNew({ ...memNew, content: e.target.value })}
                />
                <input
                  type="text"
                  value={memNew.tags}
                  placeholder="теги через запятую (необязательно)"
                  onChange={(e) => setMemNew({ ...memNew, tags: e.target.value })}
                />
                <div className="form-actions">
                  <button className="primary-btn small" type="submit">
                    Добавить
                  </button>
                </div>
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

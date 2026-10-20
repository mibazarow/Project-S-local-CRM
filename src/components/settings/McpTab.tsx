import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as ipc from "../../api/ipc";
import type { McpServerStatus } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { confirmRemove } from "../../shared/confirm";
import { Card, Field } from "./shared";
import {
  emptyMcpForm,
  parseKeyLines,
  type McpForm,
  type McpTransportKind,
} from "./forms";

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

const hasExtras = (f: McpForm) =>
  Boolean(f.args.trim() || f.env.trim() || f.headers.trim() || f.timeout.trim());

export default function McpTab({ active }: { active: boolean }) {
  const [mcp, setMcp] = useState<McpServerStatus[] | null>(null);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [mcpBusy, setMcpBusy] = useState(false);
  const [mcpForm, setMcpForm] = useState<McpForm>({ ...emptyMcpForm });
  // Original name of the server being edited; null = the form adds a new one.
  const [mcpEditing, setMcpEditing] = useState<string | null>(null);
  const [mcpFormError, setMcpFormError] = useState<string | null>(null);
  // Progressive disclosure: args/env/headers/timeout live behind a details
  // toggle, auto-opened whenever the current form actually carries them.
  const [mcpExtrasOpen, setMcpExtrasOpen] = useState(false);

  const loadMcp = useCallback(() => {
    void ipc
      .mcpStatus()
      .then(setMcp)
      .catch((e) => setMcpError(errMsg(e)));
  }, []);

  useEffect(() => {
    loadMcp();
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    // mcp.json written elsewhere (agent's mcp_add, hand edits) lands through
    // the watcher's config-changed event, so the list refreshes on its own.
    void ipc
      .onConfigChanged(({ which }) => {
        if (which === "mcp") loadMcp();
      })
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [loadMcp]);

  // The command persists enabled in mcp.json, stops/starts the server and
  // returns fresh status rows; the config-changed event also refreshes.
  const toggleMcp = async (name: string, enabled: boolean) => {
    setMcpBusy(true);
    setMcpError(null);
    try {
      setMcp(await ipc.mcpSetEnabled(name, enabled));
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
    let timeoutSecs: number | null = null;
    if (mcpForm.timeout.trim()) {
      const t = Number(mcpForm.timeout.trim().replace(",", "."));
      if (!Number.isInteger(t) || t < 1 || t > 3600) {
        setMcpFormError("Таймаут — целое число секунд от 1 до 3600");
        return;
      }
      timeoutSecs = t;
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
            ? [
                ...servers,
                {
                  name,
                  enabled: true,
                  ...(timeoutSecs !== null ? { timeoutSecs } : {}),
                  transport,
                },
              ]
            : servers.map((s) =>
                s.name === editing
                  ? {
                      ...s,
                      name,
                      ...(timeoutSecs !== null ? { timeoutSecs } : {}),
                      transport,
                    }
                  : s,
              ),
        ),
      );
      setMcpForm({ ...emptyMcpForm });
      setMcpEditing(null);
      setMcpExtrasOpen(false);
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
        const form: McpForm = {
          name,
          transport: kind,
          command: String(t.command ?? ""),
          args: Array.isArray(t.args) ? t.args.map((a) => String(a)).join("\n") : "",
          env: mapLines(t.env, "="),
          url: String(t.url ?? ""),
          headers: mapLines(t.headers, ": "),
          timeout:
            typeof entry.timeoutSecs === "number" ? String(entry.timeoutSecs) : "",
        };
        setMcpEditing(name);
        setMcpForm(form);
        setMcpExtrasOpen(hasExtras(form));
      })
      .catch((e) => setMcpFormError(errMsg(e)));
  };

  const cancelMcpEdit = () => {
    setMcpEditing(null);
    setMcpForm({ ...emptyMcpForm });
    setMcpFormError(null);
    setMcpExtrasOpen(false);
  };

  const deleteMcpServer = async (name: string) => {
    const ok = await confirmRemove({
      title: `Удалить сервер «${name}»?`,
      message: "Запись о сервере будет удалена из mcp.json.",
    });
    if (!ok) return;
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

  if (!active) return null;
  return (
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
      <Card
        title={mcpEditing === null ? "Добавить сервер" : `Изменить сервер «${mcpEditing}»`}
        desc="Сервер сохраняется в mcp.json и сразу перезапускается."
      >
        <form className="set-form" onSubmit={(e) => void saveMcpServer(e)}>
          <div className="set-form-section">
            <div className="set-form-section-title">Основное</div>
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
              <Field label="Команда" hint="Программа, запускающая сервер (npx, node, python).">
                <input
                  type="text"
                  value={mcpForm.command}
                  onChange={(e) => setMcpForm({ ...mcpForm, command: e.target.value })}
                  placeholder="npx"
                  spellCheck={false}
                />
              </Field>
            ) : (
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
            )}
          </div>
          <details
            className="set-form-details"
            open={mcpExtrasOpen}
            onToggle={(e) => setMcpExtrasOpen(e.currentTarget.open)}
          >
            <summary>Дополнительно</summary>
            {mcpForm.transport === "stdio" ? (
              <>
                <Field label="Аргументы (по одному на строку или через пробел)" wide>
                  <textarea
                    rows={2}
                    value={mcpForm.args}
                    onChange={(e) =>
                      setMcpForm({ ...mcpForm, args: e.target.value })
                    }
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
            )}
            <Field
              label="Таймаут вызова, сек (необязательно)"
              hint="По умолчанию 120 с. Целое число от 1 до 3600."
            >
              <input
                type="text"
                value={mcpForm.timeout}
                onChange={(e) => setMcpForm({ ...mcpForm, timeout: e.target.value })}
                placeholder="120"
                spellCheck={false}
              />
            </Field>
          </details>
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
    </div>
  );
}

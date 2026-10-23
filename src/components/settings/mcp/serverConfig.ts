import * as ipc from "../../../api/ipc";
import type { McpServerStatus } from "../../../api/types";
import { parseKeyLines, type McpForm, type McpTransportKind } from "../forms";

// Read-modify-write of mcp.json: unknown top-level and per-server keys
// survive, then the servers are restarted and the fresh status returned.
export async function patchMcpServers(
  mutate: (servers: Record<string, unknown>[]) => Record<string, unknown>[],
): Promise<McpServerStatus[]> {
  const raw = (await ipc.readConfig("mcp")) as Record<string, unknown>;
  const servers = Array.isArray(raw.servers) ? (raw.servers as Record<string, unknown>[]) : [];
  await ipc.writeConfig("mcp", { ...raw, servers: mutate(servers) });
  return ipc.mcpReload();
}

export const hasExtras = (f: McpForm) =>
  Boolean(f.args.trim() || f.env.trim() || f.headers.trim() || f.timeout.trim());

// Validated form → the fields a mcp.json entry needs. `takenNames` is the list
// of names that must not collide (empty while editing an existing server).
export type McpDraft =
  | { error: string }
  | { name: string; timeoutSecs: number | null; transport: Record<string, unknown> };

export function buildMcpDraft(form: McpForm, takenNames: string[]): McpDraft {
  const name = form.name.trim();
  if (!name) return { error: "Укажи имя сервера" };
  if (!/^[A-Za-z0-9][A-Za-z0-9_-]*$/.test(name)) {
    return { error: "Имя: латиница, цифры, дефис или подчёркивание, без пробелов" };
  }
  if (takenNames.includes(name)) return { error: "Сервер с таким именем уже есть" };

  let timeoutSecs: number | null = null;
  if (form.timeout.trim()) {
    const t = Number(form.timeout.trim().replace(",", "."));
    if (!Number.isInteger(t) || t < 1 || t > 3600) {
      return { error: "Таймаут — целое число секунд от 1 до 3600" };
    }
    timeoutSecs = t;
  }

  let transport: Record<string, unknown>;
  if (form.transport === "stdio") {
    const command = form.command.trim();
    if (!command) return { error: "Укажи команду — npx, node, python…" };
    const env = parseKeyLines(form.env, false);
    if (env === null) {
      return { error: "Переменные окружения: каждая строка в формате KEY=VALUE" };
    }
    transport = {
      type: "stdio",
      command,
      args: form.args.trim().split(/\s+/).filter(Boolean),
    };
    if (Object.keys(env).length) transport.env = env;
  } else {
    const url = form.url.trim();
    if (!url) return { error: "Укажи URL сервера" };
    if (!/^https?:\/\//i.test(url)) return { error: "URL должен начинаться с http:// или https://" };
    const headers = parseKeyLines(form.headers, true);
    if (headers === null) {
      return { error: "Заголовки: строки вида Authorization: Bearer … или KEY=VALUE" };
    }
    transport = { type: form.transport, url };
    if (Object.keys(headers).length) transport.headers = headers;
  }

  return { name, timeoutSecs, transport };
}

// Pre-fill the form from the raw mcp.json entry (status rows carry no
// command/url), so hand-edited servers can be fixed from the UI.
export async function readMcpForm(
  name: string,
): Promise<{ error: string } | { form: McpForm; extrasOpen: boolean }> {
  const raw = await ipc.readConfig("mcp");
  const servers = Array.isArray((raw as Record<string, unknown>).servers)
    ? ((raw as Record<string, unknown>).servers as Record<string, unknown>[])
    : [];
  const entry = servers.find((s) => s.name === name);
  if (!entry) return { error: "Сервер не найден в mcp.json — нажми «Перезагрузить»" };

  const t = (entry.transport ?? {}) as Record<string, unknown>;
  const kind: McpTransportKind = t.type === "http" || t.type === "sse" ? t.type : "stdio";
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
    timeout: typeof entry.timeoutSecs === "number" ? String(entry.timeoutSecs) : "",
  };
  return { form, extrasOpen: hasExtras(form) };
}

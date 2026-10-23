import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as ipc from "../../api/ipc";
import type { McpServerStatus } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { confirmRemove } from "../../shared/confirm";
import { emptyMcpForm, type McpForm } from "./forms";
import { McpFormCard } from "./mcp/McpFormCard";
import { McpServerList } from "./mcp/McpServerList";
import { buildMcpDraft, patchMcpServers, readMcpForm } from "./mcp/serverConfig";

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
    const takenNames = mcpEditing === null ? (mcp?.map((s) => s.name) ?? []) : [];
    const draft = buildMcpDraft(mcpForm, takenNames);
    if ("error" in draft) {
      setMcpFormError(draft.error);
      return;
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
                  name: draft.name,
                  enabled: true,
                  ...(draft.timeoutSecs !== null ? { timeoutSecs: draft.timeoutSecs } : {}),
                  transport: draft.transport,
                },
              ]
            : servers.map((s) =>
                s.name === editing
                  ? {
                      ...s,
                      name: draft.name,
                      ...(draft.timeoutSecs !== null ? { timeoutSecs: draft.timeoutSecs } : {}),
                      transport: draft.transport,
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

  const startMcpEdit = (name: string) => {
    setMcpError(null);
    setMcpFormError(null);
    void readMcpForm(name)
      .then((res) => {
        if ("error" in res) {
          setMcpFormError(res.error);
          return;
        }
        setMcpEditing(name);
        setMcpForm(res.form);
        setMcpExtrasOpen(res.extrasOpen);
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
      <McpFormCard
        form={mcpForm}
        onFormChange={setMcpForm}
        editing={mcpEditing}
        busy={mcpBusy}
        error={mcpFormError}
        extrasOpen={mcpExtrasOpen}
        onExtrasOpenChange={setMcpExtrasOpen}
        onSubmit={(e) => void saveMcpServer(e)}
        onCancel={cancelMcpEdit}
      />
      <McpServerList
        servers={mcp}
        error={mcpError}
        busy={mcpBusy}
        onReload={reloadMcp}
        onEdit={startMcpEdit}
        onDelete={(name) => void deleteMcpServer(name)}
        onToggle={(name, enabled) => void toggleMcp(name, enabled)}
      />
    </div>
  );
}

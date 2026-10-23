import type { McpServerStatus } from "../../../api/types";
import { Card } from "../shared";

export function McpServerList({
  servers,
  error,
  busy,
  onReload,
  onEdit,
  onDelete,
  onToggle,
}: {
  servers: McpServerStatus[] | null;
  error: string | null;
  busy: boolean;
  onReload: () => void;
  onEdit: (name: string) => void;
  onDelete: (name: string) => void;
  onToggle: (name: string, enabled: boolean) => void;
}) {
  return (
    <Card title="Серверы" desc="Перечитывает mcp.json и перезапускает серверы.">
      <div className="set-actions">
        <button className="ghost-btn" disabled={busy} onClick={onReload}>
          Перезагрузить
        </button>
      </div>
      {servers === null ? (
        <div className="set-empty">Загрузка…</div>
      ) : servers.length === 0 ? (
        <div className="set-empty">Серверов нет. Добавь первый ниже.</div>
      ) : (
        <div className="set-list">
          {servers.map((s, i) => (
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
                <button className="ghost-btn small" onClick={() => onEdit(s.name)}>
                  Изменить
                </button>
                <button className="ghost-btn small" onClick={() => onDelete(s.name)}>
                  Удалить
                </button>
              </span>
              <label className="switch">
                <input
                  type="checkbox"
                  checked={s.enabled}
                  disabled={busy}
                  onChange={(e) => onToggle(s.name, e.target.checked)}
                />
                <span className="track"></span>
              </label>
            </div>
          ))}
        </div>
      )}
      {error && <div className="set-error">{error}</div>}
    </Card>
  );
}

import { useCallback, useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type { JournalEntryDto } from "../../api/types";
import { toMessage } from "../chat/chatShared";

const KIND_LABEL: Record<string, string> = {
  project: "Проект",
  harness: "Харнес",
  schedule: "Расписание",
};

const DAY_OPTIONS = [7, 14, 30, 90];

function formatTime(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  return d.toLocaleString("ru-RU", {
    day: "numeric",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

function formatDay(date: string): string {
  const d = new Date(`${date}T00:00:00`);
  if (Number.isNaN(d.getTime())) return date;
  return d.toLocaleDateString("ru-RU", { day: "numeric", month: "short" });
}

function runLabel(e: JournalEntryDto): string {
  const run =
    e.projectId != null
      ? `Проект #${e.projectId}`
      : e.sessionId != null
        ? `Сессия #${e.sessionId}`
        : `Чат #${e.chatId}`;
  return `${run} · шаг ${e.step}`;
}

const RefreshIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M21 12a9 9 0 1 1-2.64-6.36M21 3v6h-6" />
  </svg>
);

export default function JournalView() {
  const [days, setDays] = useState(7);
  const [kind, setKind] = useState("");
  const [tool, setTool] = useState("");
  const [onlyErrors, setOnlyErrors] = useState(false);
  const [entries, setEntries] = useState<JournalEntryDto[]>([]);
  const [tools, setTools] = useState<string[]>([]);
  const [logDays, setLogDays] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [expanded, setExpanded] = useState<number | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const res = await ipc.journalList({
        days,
        kind: kind || undefined,
        tool: tool || undefined,
        ok: onlyErrors ? false : undefined,
      });
      setEntries(res.entries);
      setTools(res.tools);
      setLogDays(res.days);
      setExpanded(null);
      setError(null);
    } catch (e) {
      setError(toMessage(e));
    } finally {
      setLoading(false);
    }
  }, [days, kind, tool, onlyErrors]);

  // Refresh on mount, on filter change and on the button — no live polling.
  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <main className="view journal-view active">
      <section className="center">
        <div className="center-head">
          <h2>Журнал</h2>
          <div className="journal-filters">
            <select value={days} onChange={(e) => setDays(Number(e.target.value))} title="Период">
              {DAY_OPTIONS.map((d) => (
                <option key={d} value={d}>
                  {d} дн.
                </option>
              ))}
            </select>
            <select value={kind} onChange={(e) => setKind(e.target.value)} title="Тип запуска">
              <option value="">Все запуски</option>
              <option value="project">Проекты</option>
              <option value="harness">Харнес</option>
              <option value="schedule">Расписания</option>
            </select>
            <select value={tool} onChange={(e) => setTool(e.target.value)} title="Инструмент">
              <option value="">Все инструменты</option>
              {tools.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
            <label className="journal-toggle">
              Только ошибки
              <span className="switch">
                <input
                  type="checkbox"
                  checked={onlyErrors}
                  onChange={(e) => setOnlyErrors(e.target.checked)}
                />
                <span className="track" />
              </span>
            </label>
            <button className="ghost-btn small" type="button" disabled={loading} onClick={() => void refresh()}>
              <RefreshIcon />
              Обновить
            </button>
          </div>
        </div>

        {error && <div className="chat-error">{error}</div>}

        <div className="journal-list">
          {entries.length === 0 && !error && <div className="journal-empty">Записей нет</div>}
          {entries.map((e, i) => (
            <div
              key={i}
              className="journal-row"
              role="button"
              tabIndex={0}
              onClick={() => setExpanded(expanded === i ? null : i)}
              onKeyDown={(ev) => {
                if (ev.key === "Enter") setExpanded(expanded === i ? null : i);
              }}
            >
              <div className="journal-row-head">
                <span className="journal-time">{formatTime(e.ts)}</span>
                <span className="badge">{KIND_LABEL[e.kind] ?? e.kind}</span>
                <span className="journal-run">{runLabel(e)}</span>
                <span className="journal-tool">{e.tool}</span>
                <span className={`badge ${e.ok ? "j-ok" : "j-err"}`}>{e.ok ? "ok" : (e.code ?? "ошибка")}</span>
                <span className="journal-ms">{e.ms} мс</span>
              </div>
              {expanded === i ? (
                <div className="journal-detail">
                  <span className="lbl">Аргументы</span>
                  <pre>{e.args}</pre>
                  <span className="lbl">Результат</span>
                  <pre>{e.result}</pre>
                </div>
              ) : (
                <div className="journal-preview">
                  {e.args} · {e.result}
                </div>
              )}
            </div>
          ))}
        </div>

        <div className="sidebar-foot muted-note">
          Логи есть за: {logDays.slice(0, 7).map(formatDay).join(", ") || "—"}
        </div>
      </section>
    </main>
  );
}

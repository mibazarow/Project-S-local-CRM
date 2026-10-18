import { useState, type FormEvent } from "react";
import * as ipc from "../../api/ipc";
import { toMessage } from "../chat/chatShared";
import type { ScheduleDto } from "../../api/types";
import { formatInstant } from "../../shared/format";
import { TrashIcon } from "./shared";

const KIND_LABEL: Record<string, string> = {
  run_at: "Однократно",
  cron: "Cron",
  notify: "Уведомление",
};

const STATUS_LABEL: Record<string, string> = {
  active: "активно",
  paused: "пауза",
  done: "готово",
  error: "ошибка",
};

const SPEC_PLACEHOLDER: Record<string, string> = {
  run_at: "2026-10-20T10:00:00",
  cron: "0 10 * * *",
  notify: "2026-10-20T10:00:00",
};

const SPEC_HINT: Record<string, string> = {
  run_at: "ISO-время, напр. 2026-10-20T10:00:00",
  cron: "5 полей: минута час день месяц день недели",
  notify: "ISO-время, напр. 2026-10-20T10:00:00",
};

const GROUPS: Array<{ status: ScheduleDto["status"]; title: string }> = [
  { status: "active", title: "Активные" },
  { status: "paused", title: "Пауза" },
  { status: "error", title: "Ошибки" },
  { status: "done", title: "Завершённые" },
];

function ScheduleItem({
  schedule,
  onToggle,
  onDelete,
  onOpen,
}: {
  schedule: ScheduleDto;
  onToggle: (s: ScheduleDto) => void;
  onDelete: (s: ScheduleDto) => void;
  onOpen: (s: ScheduleDto) => void;
}) {
  const s = schedule;
  return (
    <div
      className="schedule-item"
      role="button"
      tabIndex={0}
      title="Открыть чат расписания"
      onClick={() => onOpen(s)}
      onKeyDown={(e) => {
        if (e.key === "Enter") onOpen(s);
      }}
    >
      <div className="info">
        <div className="t">{s.title ?? s.spec}</div>
        <div className="c">
          {KIND_LABEL[s.kind] ?? s.kind}
          {" · "}
          {s.timezone ?? "часовой пояс системы"}
          {s.nextRunAt ? ` · след.: ${formatInstant(s.nextRunAt)}` : ""}
        </div>
        {s.error && <div className="c">{s.error}</div>}
      </div>
      <span className={`badge st-${s.status}`}>{STATUS_LABEL[s.status] ?? s.status}</span>
      <label
        className="switch"
        title={s.status === "active" ? "Поставить на паузу" : "Возобновить"}
        onClick={(e) => e.stopPropagation()}
      >
        <input
          type="checkbox"
          checked={s.status === "active"}
          disabled={s.status === "done"}
          onChange={() => onToggle(s)}
        />
        <span className="track" />
      </label>
      <button
        className="mini-btn danger"
        type="button"
        title="Удалить расписание"
        onClick={(e) => {
          e.stopPropagation();
          onDelete(s);
        }}
      >
        <TrashIcon />
      </button>
    </div>
  );
}

export default function SchedulePanel({
  schedules,
  schedulesError,
  activeId,
  activeSessionTitle,
  onToggle,
  onDelete,
  onOpen,
  onReload,
}: {
  schedules: ScheduleDto[];
  schedulesError: string | null;
  activeId: number | null;
  activeSessionTitle: string | null;
  onToggle: (s: ScheduleDto) => void;
  onDelete: (s: ScheduleDto) => void;
  onOpen: (s: ScheduleDto) => void;
  onReload: () => Promise<void>;
}) {
  const [formOpen, setFormOpen] = useState(false);
  const [kind, setKind] = useState<ScheduleDto["kind"]>("run_at");
  const [spec, setSpec] = useState("");
  const [prompt, setPrompt] = useState("");
  const [timezone, setTimezone] = useState("");
  const [formError, setFormError] = useState<string | null>(null);

  const addSchedule = async (e: FormEvent) => {
    e.preventDefault();
    setFormError(null);
    try {
      await ipc.scheduleCreate(
        kind,
        spec.trim(),
        prompt.trim() || null,
        timezone.trim() || null,
        activeId,
      );
      setFormOpen(false);
      setSpec("");
      setPrompt("");
      setTimezone("");
      await onReload();
    } catch (err) {
      setFormError(toMessage(err));
    }
  };

  return (
    <aside className="chat">
      <div className="chat-head">
        <span className="chat-title">Расписания</span>
        <button
          className="icon-btn small"
          type="button"
          title={formOpen ? "Закрыть форму" : "Добавить расписание"}
          onClick={() => {
            setFormOpen((v) => !v);
            setFormError(null);
          }}
        >
          <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><path d="M12 5v14M5 12h14" /></svg>
        </button>
      </div>

      {formOpen && (
        <form className="schedule-form" onSubmit={(e) => void addSchedule(e)}>
          <div className="set-row">
            <label>Тип</label>
            <select value={kind} onChange={(e) => setKind(e.target.value as ScheduleDto["kind"])}>
              <option value="run_at">Однократно (run_at)</option>
              <option value="cron">Повтор по расписанию (cron)</option>
              <option value="notify">Уведомление (notify)</option>
            </select>
          </div>
          <div className="set-row">
            <label>Расписание</label>
            <input
              value={spec}
              onChange={(e) => setSpec(e.target.value)}
              placeholder={SPEC_PLACEHOLDER[kind]}
              required
            />
            <span className="set-hint">{SPEC_HINT[kind]}</span>
          </div>
          <div className="set-row">
            <label>{kind === "notify" ? "Текст уведомления" : "Поручение агенту"}</label>
            <textarea
              rows={2}
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              placeholder={kind === "notify" ? "Напомнить о…" : "Что сделать агенту…"}
            />
          </div>
          <div className="set-row">
            <label>Часовой пояс (необязательно)</label>
            <input value={timezone} onChange={(e) => setTimezone(e.target.value)} placeholder="Europe/Moscow" />
          </div>
          <span className="set-hint">
            {activeSessionTitle !== null
              ? `run_at и cron запустятся в сессии «${activeSessionTitle}»`
              : "Откройте сессию — run_at и cron запустятся в ней"}
          </span>
          {formError && <span className="set-error">{formError}</span>}
          <div className="form-actions">
            <button className="primary-btn small" type="submit">
              Добавить
            </button>
            <button className="ghost-btn small" type="button" onClick={() => setFormOpen(false)}>
              Отмена
            </button>
          </div>
        </form>
      )}

      <div className="schedule-list">
        {schedulesError && <div className="set-error">{schedulesError}</div>}
        {schedules.length === 0 && !schedulesError && (
          <div className="empty-note">Расписаний пока нет. Нажмите «+», чтобы добавить.</div>
        )}
        {GROUPS.map((g) => {
          const items = schedules.filter((s) => s.status === g.status);
          if (items.length === 0) return null;
          return (
            <div key={g.status} className="schedule-group">
              <div className="schedule-group-title">{g.title}</div>
              {items.map((s) => (
                <ScheduleItem
                  key={s.id}
                  schedule={s}
                  onToggle={onToggle}
                  onDelete={onDelete}
                  onOpen={onOpen}
                />
              ))}
            </div>
          );
        })}
      </div>
    </aside>
  );
}

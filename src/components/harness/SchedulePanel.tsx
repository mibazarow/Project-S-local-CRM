import type { ScheduleDto } from "../../api/types";
import { formatInstant } from "../../shared/format";
import { TrashIcon } from "./shared";

const KIND_LABEL: Record<string, string> = {
  run_at: "Однократно",
  cron: "Cron",
  notify: "Уведомление",
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
  onToggle,
  onDelete,
  onOpen,
  onCreate,
}: {
  schedules: ScheduleDto[];
  schedulesError: string | null;
  onToggle: (s: ScheduleDto) => void;
  onDelete: (s: ScheduleDto) => void;
  onOpen: (s: ScheduleDto) => void;
  onCreate: () => void;
}) {
  return (
    <aside className="chat">
      <div className="chat-head">
        <span className="chat-title">Расписания</span>
        <button
          className="icon-btn small schedule-add-btn"
          type="button"
          title="Создать расписание в чате"
          onClick={onCreate}
        >
          <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><path d="M12 5v14M5 12h14" /></svg>
        </button>
      </div>

      <div className="schedule-list">
        {schedulesError && <div className="set-error">{schedulesError}</div>}
        {schedules.length === 0 && !schedulesError && (
          <div className="empty-note">
            Расписаний пока нет. Нажмите «+» — расписание создаётся словами в чате с агентом.
          </div>
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

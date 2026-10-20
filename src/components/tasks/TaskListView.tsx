import { useEffect, useMemo, useState } from "react";
import type { TaskDto } from "../../api/types";
import { dueMs, formatDue, formatSpent } from "../../shared/format";

const STATUSES = ["new", "progress", "done"] as const;

interface Props {
  tasks: TaskDto[];
  columns: string[];
  overdueOnly: boolean;
}

// Group order: «Сейчас» → «Просрочено» → «Новые» → «В работе» → «Готово».
// Within a group: priority first, then nearest deadline, then position, then id.
const sortTasks = (a: TaskDto, b: TaskDto): number => {
  if (a.isPriority !== b.isPriority) return a.isPriority ? -1 : 1;
  const da = a.dueDate ? dueMs(a.dueDate, a.dueTime) : Number.POSITIVE_INFINITY;
  const db = b.dueDate ? dueMs(b.dueDate, b.dueTime) : Number.POSITIVE_INFINITY;
  if (da !== db) return da - db;
  if (a.position !== b.position) return a.position - b.position;
  return a.id - b.id;
};

function TaskRow({
  task,
  statusLabel,
  active,
}: {
  task: TaskDto;
  statusLabel: string;
  active?: boolean;
}) {
  const running = task.status === "progress" && task.startedAt !== null;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!running) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [running]);
  const sessionMs = running ? Math.max(0, now - (task.startedAt as number)) : 0;
  const totalMs = task.spentMs + sessionMs;
  const doneCount = task.checklist.filter((i) => i.done).length;

  const classes = ["task-list-row"];
  if (active) classes.push("active");
  if (task.isPriority) classes.push("priority");
  if (task.status === "done") classes.push("done");
  if (task.overdue) classes.push("overdue");

  return (
    <div className={classes.join(" ")} data-id={task.id}>
      <span className={`task-list-dot ${task.status}`} />
      <span className="task-list-status">{statusLabel}</span>
      <span className="task-list-title" title={task.title}>
        {task.title}
      </span>
      {task.isPriority && (
        <span className="task-list-flag" title="Важная">
          ⚑
        </span>
      )}
      {task.dueDate && (
        <span className={task.overdue ? "task-list-due overdue" : "task-list-due"}>
          {formatDue(task.dueDate)} {task.dueTime}
        </span>
      )}
      {task.checklist.length > 0 && (
        <span className="task-list-check">
          <span className="task-list-check-bar">
            <span
              className="task-list-check-fill"
              style={{ width: `${Math.round((doneCount / task.checklist.length) * 100)}%` }}
            />
          </span>
          <span className="task-list-check-count">
            {doneCount}/{task.checklist.length}
          </span>
        </span>
      )}
      {(running || totalMs > 0) && (
        <span className="task-list-spent">
          {active ? `${formatSpent(sessionMs)} · ${formatSpent(totalMs)}` : formatSpent(totalMs)}
        </span>
      )}
    </div>
  );
}

export default function TaskListView({ tasks, columns, overdueOnly }: Props) {
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({ done: true });

  const groups = useMemo(() => {
    const isNow = (t: TaskDto) => t.status === "progress" && t.startedAt !== null;
    const now = tasks.filter(isNow);
    const rest = tasks.filter((t) => !isNow(t));
    const overdue = rest.filter((t) => t.overdue);
    const fresh = rest.filter((t) => !t.overdue);
    const group = (status: string) =>
      fresh.filter((t) => t.status === status).sort(sortTasks);
    return {
      now: now.sort(sortTasks),
      overdue: overdue.sort(sortTasks),
      news: group("new"),
      progress: group("progress"),
      done: group("done"),
    };
  }, [tasks]);

  const statusLabel = (status: string) =>
    columns[STATUSES.indexOf(status as (typeof STATUSES)[number])] ?? status;

  if (tasks.length === 0) {
    return (
      <div className="task-list-view">
        <div className="task-list-empty">
          {overdueOnly ? "Нет просроченных задач" : "Пока нет задач"}
        </div>
      </div>
    );
  }

  const renderGroup = (key: string, label: string, list: TaskDto[]) => {
    if (list.length === 0) return null;
    const isCollapsed = collapsed[key] === true;
    return (
      <section className="task-list-group" key={key}>
        <button
          type="button"
          className={isCollapsed ? "task-list-group-head collapsed" : "task-list-group-head"}
          onClick={() => setCollapsed((prev) => ({ ...prev, [key]: !prev[key] }))}
        >
          <span className="task-list-caret" />
          <span className="task-list-group-label">{label}</span>
          <span className="count">{list.length}</span>
        </button>
        {!isCollapsed && (
          <div className="task-list-rows">
            {list.map((t) => (
              <TaskRow key={t.id} task={t} statusLabel={statusLabel(t.status)} />
            ))}
          </div>
        )}
      </section>
    );
  };

  const allDone =
    groups.now.length === 0 &&
    groups.overdue.length === 0 &&
    groups.news.length === 0 &&
    groups.progress.length === 0 &&
    groups.done.length > 0;

  return (
    <div className="task-list-view">
      {groups.now.length > 0 && (
        <section className="task-list-now">
          <div className="task-list-now-head">
            <span className="task-list-group-label">Сейчас</span>
            <span className="count">{groups.now.length}</span>
          </div>
          <div className="task-list-rows">
            {groups.now.map((t) => (
              <TaskRow
                key={t.id}
                task={t}
                statusLabel={statusLabel(t.status)}
                active
              />
            ))}
          </div>
        </section>
      )}
      {allDone && <div className="task-list-empty">Все задачи выполнены</div>}
      {renderGroup("overdue", "Просрочено", groups.overdue)}
      {renderGroup("news", statusLabel("new"), groups.news)}
      {renderGroup("progress", statusLabel("progress"), groups.progress)}
      {renderGroup("done", statusLabel("done"), groups.done)}
    </div>
  );
}

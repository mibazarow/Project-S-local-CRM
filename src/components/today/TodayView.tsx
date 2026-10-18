import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import * as ipc from "../../api/ipc";
import { useAppActions } from "../../state/AppStore";
import { usePointerDrag, type DropTarget } from "../../hooks/usePointerDrag";
import type { FsEntry, ObjectiveDto, ProjectDto, ScheduleDto, SessionDto, TaskDto } from "../../api/types";
import { toMessage } from "../chat/chatShared";
import { dueMs, formatActivity, formatDue, formatInstant, formatModified } from "../../shared/format";
import type { Tab } from "../TopBar";

interface Props {
  onTab: (tab: Tab) => void;
}

interface TaskWithProject {
  task: TaskDto;
  project: ProjectDto;
}

interface TodayData {
  overdue: TaskWithProject[];
  soon: TaskWithProject[];
  schedules: ScheduleDto[];
  objectives: ObjectiveDto[];
  notes: FsEntry[];
  sessions: SessionDto[];
}

const WEEK_MS = 7 * 24 * 60 * 60 * 1000;

// Stable ids of the six Today cards; the array is the default order.
const CARD_IDS = ["overdue", "soon", "schedules", "objectives", "notes", "chats"] as const;
// Global (not per-project) card order, kept in localStorage per SPEC 4.3.
const ORDER_KEY = "today-card-order";

function readCardOrder(): string[] {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(ORDER_KEY) ?? "[]");
    const saved = Array.isArray(raw)
      ? raw.filter((x): x is string => typeof x === "string" && (CARD_IDS as readonly string[]).includes(x))
      : [];
    const unique = [...new Set(saved)];
    return [...unique, ...CARD_IDS.filter((id) => !unique.includes(id))];
  } catch {
    return [...CARD_IDS];
  }
}

const OBJECTIVE_STATUS_LABEL: Record<string, string> = {
  active: "в работе",
  paused: "пауза",
  done: "готово",
  error: "ошибка",
};

function todayLabel(): string {
  const s = new Date().toLocaleDateString("ru-RU", { weekday: "long", day: "numeric", month: "long" });
  return s.charAt(0).toUpperCase() + s.slice(1);
}

// Русская плюрализация счётчика (задача / задачи / задач).
function plural(n: number, one: string, few: string, many: string): string {
  const m10 = n % 10;
  const m100 = n % 100;
  if (m10 === 1 && m100 !== 11) return one;
  if (m10 >= 2 && m10 <= 4 && (m100 < 10 || m100 >= 20)) return few;
  return many;
}

// Короткая строка-состояние под датой: куда смотреть в первую очередь.
function greetingLine(d: TodayData): string {
  if (d.overdue.length) {
    return `Просрочено ${d.overdue.length} ${plural(d.overdue.length, "задача", "задачи", "задач")} — начните с них.`;
  }
  if (d.soon.length) {
    return `На ближайшую неделю — ${d.soon.length} ${plural(d.soon.length, "задача", "задачи", "задач")}.`;
  }
  if (d.schedules.length) {
    return `Просроченных задач нет. Активных расписаний: ${d.schedules.length}.`;
  }
  return "Просроченных задач нет. На сегодня всё спокойно.";
}

const RefreshIcon = () => (
  <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M21 12a9 9 0 1 1-2.64-6.36M21 3v6h-6" />
  </svg>
);

const IconAlert = () => (
  <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M12 3.5 2.8 19.5h18.4L12 3.5z" />
    <path d="M12 10v4M12 17h.01" />
  </svg>
);

const IconCalendar = () => (
  <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <rect x="3" y="5" width="18" height="16" rx="2" />
    <path d="M8 3v4M16 3v4M3 10h18" />
  </svg>
);

const IconClock = () => (
  <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <circle cx="12" cy="12" r="9" />
    <path d="M12 7.5V12l3 2" />
  </svg>
);

const IconTarget = () => (
  <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <circle cx="12" cy="12" r="9" />
    <circle cx="12" cy="12" r="5" />
    <circle cx="12" cy="12" r="1.4" />
  </svg>
);

const IconNote = () => (
  <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5z" />
    <path d="M14 3v5h5M9 13h6M9 17h4" />
  </svg>
);

const IconChat = () => (
  <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
  </svg>
);

const GripIcon = () => (
  <svg viewBox="0 0 24 24" width="13" height="13" fill="currentColor">
    <circle cx="9" cy="5.5" r="1.4" />
    <circle cx="15" cy="5.5" r="1.4" />
    <circle cx="9" cy="12" r="1.4" />
    <circle cx="15" cy="12" r="1.4" />
    <circle cx="9" cy="18.5" r="1.4" />
    <circle cx="15" cy="18.5" r="1.4" />
  </svg>
);

interface CardProps {
  cardId: string;
  icon: ReactNode;
  title: string;
  count?: number;
  danger?: boolean;
  action?: { label: string; onClick: () => void };
  dragging?: boolean;
  onDragStart: (e: ReactPointerEvent<HTMLElement>, id: string) => void;
  children: ReactNode;
}

function Card({ cardId, icon, title, count, danger, action, dragging, onDragStart, children }: CardProps) {
  return (
    <section
      className={`today-card${dragging ? " dragging" : ""}`}
      data-card-id={cardId}
      onPointerDown={(e) => {
        // Reorder drags start on the header only; buttons/inputs inside the
        // card body keep their own behavior.
        if ((e.target as HTMLElement).closest(".today-card-head")) onDragStart(e, cardId);
      }}
    >
      <header className="today-card-head">
        <span className="today-card-grip" aria-hidden="true">
          <GripIcon />
        </span>
        <span className={`today-card-icon${danger ? " is-danger" : ""}`}>{icon}</span>
        <h3 className="today-card-title">{title}</h3>
        {count != null && (
          <span className={`today-count${count === 0 ? " is-zero" : ""}`}>{count}</span>
        )}
        {action && (
          <button className="today-card-all" type="button" onClick={action.onClick}>
            {action.label}
          </button>
        )}
      </header>
      <div className="today-list">{children}</div>
    </section>
  );
}

function TaskRow({
  twp,
  overdue,
  onOpen,
}: {
  twp: TaskWithProject;
  overdue?: boolean;
  onOpen: (projectId: number, taskId: number) => void;
}) {
  const { task, project } = twp;
  return (
    <div
      className="today-row"
      role="button"
      tabIndex={0}
      onClick={() => onOpen(project.id, task.id)}
      onKeyDown={(e) => {
        if (e.key === "Enter") onOpen(project.id, task.id);
      }}
    >
      <span className="today-row-title">{task.title}</span>
      <span className="today-row-meta">
        <span className="today-row-proj">{project.name}</span>
        <span className={`today-row-date${overdue ? " is-overdue" : ""}`}>{formatDue(task.dueDate!)}</span>
        {task.dueTime && <span className="today-row-time">{task.dueTime}</span>}
      </span>
    </div>
  );
}

export default function TodayView({ onTab }: Props) {
  const actions = useAppActions();
  const [data, setData] = useState<TodayData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Record<number, boolean>>({});
  const [newTitle, setNewTitle] = useState("");
  const [newSteps, setNewSteps] = useState("");

  const gridRef = useRef<HTMLDivElement>(null);
  const [order, setOrder] = useState<string[]>(readCardOrder);

  // Card reorder drag: insertion index = nearest card, before/after by its
  // vertical midline (works for both 1- and 2-column grid layouts).
  const { draggingId, onPointerDown } = usePointerDrag<string>({
    resolveTarget: (clientX, clientY, id): DropTarget<string> | null => {
      const grid = gridRef.current;
      if (!grid) return null;
      const cards = Array.from(
        grid.querySelectorAll<HTMLElement>(".today-card[data-card-id]"),
      ).filter((el) => el.dataset.cardId !== id);
      let index = 0;
      let best = Infinity;
      cards.forEach((el, i) => {
        const r = el.getBoundingClientRect();
        const d = Math.hypot(clientX - (r.left + r.width / 2), clientY - (r.top + r.height / 2));
        if (d < best) {
          best = d;
          index = clientY > r.top + r.height / 2 ? i + 1 : i;
        }
      });
      return { container: "grid", index, draggingId: id };
    },
    onDrop: (id, target) => {
      setOrder((prev) => {
        const others = prev.filter((x) => x !== id);
        const next = [...others];
        next.splice(Math.max(0, Math.min(target.index, others.length)), 0, id);
        try {
          localStorage.setItem(ORDER_KEY, JSON.stringify(next));
        } catch {
          // Storage unavailable: the order simply won't persist across restarts.
        }
        return next;
      });
    },
  });

  // Sequence guard: overlapping reloads (events, focus, manual) must never let
  // a stale response overwrite a newer one.
  const seq = useRef(0);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const reload = useCallback(async () => {
    const s = ++seq.current;
    try {
      const [projects, schedulesAll, objectivesAll, notesAll, sessionsAll] = await Promise.all([
        ipc.listProjects(),
        ipc.scheduleList(),
        ipc.objectiveList(),
        ipc.notesTree(),
        ipc.harnessListSessions(),
      ]);
      const tasksPer = await Promise.all(
        projects.map((p) => ipc.tasksList(p.id).catch(() => [] as TaskDto[])),
      );
      if (s !== seq.current) return;

      const startOfDay = new Date();
      startOfDay.setHours(0, 0, 0, 0);
      const weekEnd = startOfDay.getTime() + WEEK_MS;

      const overdue: TaskWithProject[] = [];
      const soon: TaskWithProject[] = [];
      const nowMs = Date.now();
      projects.forEach((project, i) => {
        for (const task of tasksPer[i]) {
          if (task.status === "done" || !task.dueDate) continue;
          // Просрочка — по дате + времени дедлайна (SPEC 4.3).
          const due = dueMs(task.dueDate, task.dueTime);
          if (Number.isNaN(due)) continue;
          if (due < nowMs) overdue.push({ task, project });
          else if (due <= weekEnd) soon.push({ task, project });
        }
      });
      const byDue = (a: TaskWithProject, b: TaskWithProject) =>
        (a.task.dueDate ?? "").localeCompare(b.task.dueDate ?? "") ||
        (a.task.dueTime ?? "").localeCompare(b.task.dueTime ?? "");
      overdue.sort(byDue);
      soon.sort(byDue);

      const schedules = schedulesAll
        .filter((sc) => sc.status === "active")
        .sort((a, b) => (a.nextRunAt ?? "9999").localeCompare(b.nextRunAt ?? "9999"));

      const notes = notesAll
        .filter((e) => e.kind === "file" && e.isMarkdown)
        .sort((a, b) => b.modified - a.modified)
        .slice(0, 8);

      const sessions = [...sessionsAll]
        .sort((a, b) => b.lastActiveAt.localeCompare(a.lastActiveAt))
        .slice(0, 6);

      setData({ overdue, soon, schedules, objectives: objectivesAll, notes, sessions });
      setError(null);
    } catch (e) {
      if (s === seq.current) setError(toMessage(e));
    }
  }, []);

  // Debounced + deduped: while a trailing reload is already queued, further
  // change events fold into it instead of stacking requests.
  const scheduleReload = useCallback(() => {
    if (timer.current) return;
    timer.current = setTimeout(() => {
      timer.current = null;
      void reload();
    }, 250);
  }, [reload]);

  useEffect(() => {
    void reload();
    let cancelled = false;
    const unsubs: Array<() => void> = [];
    const sub = (p: Promise<() => void>) => void p.then((u) => (cancelled ? u() : unsubs.push(u)));
    sub(ipc.onProjectsChanged(scheduleReload));
    sub(ipc.onTasksChanged(scheduleReload));
    sub(ipc.onSchedulesChanged(scheduleReload));
    sub(ipc.onObjectivesChanged(scheduleReload));
    sub(ipc.onNotesChanged(scheduleReload));
    const onFocus = () => scheduleReload();
    window.addEventListener("focus", onFocus);
    return () => {
      cancelled = true;
      unsubs.forEach((u) => u());
      window.removeEventListener("focus", onFocus);
      if (timer.current) clearTimeout(timer.current);
    };
  }, [reload, scheduleReload]);

  // Reuse the command-palette task navigation path: open the project, ask
  // FilesPanel to switch to «Задачи» and focus (raise + pulse) this task.
  const openTask = (projectId: number, taskId: number) => {
    void actions.openProject(projectId);
    actions.requestTask(taskId);
    onTab("projects");
  };

  // Same behavior as the Harness schedule click: open the schedule's chat,
  // ask the agent to explain the task there (the send persists the user turn
  // before returning, so the Harness transcript shows it on mount).
  const openSchedule = async (s: ScheduleDto) => {
    try {
      const { sessionId } = await ipc.scheduleOpen(s.id);
      const question = `Коротко (1–2 предложения) объясни, что это за задача по расписанию: «${s.title ?? s.spec}» (id ${s.id}).`;
      try {
        await ipc.sendSessionMessage(sessionId, question);
      } catch {
        // A run may already be live in that session: just open the chat.
      }
      actions.requestSession(sessionId);
      onTab("harness");
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const openNote = (relPath: string) => {
    actions.requestNote(relPath);
    onTab("notes");
  };

  const openSession = (id: number) => {
    actions.requestSession(id);
    onTab("harness");
  };

  // Objective mutations emit "objectives-changed", so the debounced reload
  // refreshes the list; the handlers only report errors and clear the form.
  const addObjective = async () => {
    const title = newTitle.trim();
    if (!title) return;
    const steps = newSteps
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    try {
      await ipc.objectiveCreate(title, steps.length ? steps : undefined);
      setNewTitle("");
      setNewSteps("");
      setError(null);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const toggleObjective = (id: number) =>
    setExpanded((prev) => ({ ...prev, [id]: !prev[id] }));

  const toggleStep = async (objectiveId: number, stepId: number, done: boolean) => {
    try {
      await ipc.objectiveStepDone(objectiveId, stepId, done);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const setObjectiveStatus = async (id: number, status: string) => {
    try {
      await ipc.objectiveUpdate(id, { status });
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const activeGoals = data ? data.objectives.filter((o) => o.status !== "done").length : 0;

  // Keyed by card id: reordering moves DOM nodes without remounting contents
  // (expanded objectives and the new-objective form survive a reorder).
  const cardNodes: Record<string, ReactNode> | null = data && {
    overdue: (
      <Card
        key="overdue"
        cardId="overdue"
        dragging={draggingId === "overdue"}
        onDragStart={onPointerDown}
        icon={<IconAlert />}
        title="Просрочено"
        count={data.overdue.length}
        danger={data.overdue.length > 0}
      >
        {data.overdue.length === 0 && (
          <div className="today-empty">Просроченных задач нет.</div>
        )}
        {data.overdue.map((twp) => (
          <TaskRow key={`o-${twp.task.id}`} twp={twp} overdue onOpen={openTask} />
        ))}
      </Card>
    ),
    soon: (
      <Card
        key="soon"
        cardId="soon"
        dragging={draggingId === "soon"}
        onDragStart={onPointerDown}
        icon={<IconCalendar />}
        title="Скоро (7 дней)"
        count={data.soon.length}
      >
        {data.soon.length === 0 && (
          <div className="today-empty">На ближайшую неделю задач нет.</div>
        )}
        {data.soon.map((twp) => (
          <TaskRow key={`s-${twp.task.id}`} twp={twp} onOpen={openTask} />
        ))}
      </Card>
    ),
    schedules: (
      <Card
        key="schedules"
        cardId="schedules"
        dragging={draggingId === "schedules"}
        onDragStart={onPointerDown}
        icon={<IconClock />}
        title="Расписания"
        count={data.schedules.length}
        action={{ label: "все", onClick: () => onTab("harness") }}
      >
        {data.schedules.length === 0 && (
          <div className="today-empty">
            Активных расписаний нет. Создайте их во вкладке «Харнес».
          </div>
        )}
        {data.schedules.map((s) => (
          <div
            className="today-row"
            key={`sc-${s.id}`}
            role="button"
            tabIndex={0}
            title="Открыть чат расписания"
            onClick={() => void openSchedule(s)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void openSchedule(s);
            }}
          >
            <span className="today-row-title">{s.title ?? s.spec}</span>
            <span className="today-row-meta">
              <span className="today-row-date">
                {s.nextRunAt ? `след.: ${formatInstant(s.nextRunAt)}` : "без запуска"}
              </span>
            </span>
          </div>
        ))}
      </Card>
    ),
    objectives: (
      <Card
        key="objectives"
        cardId="objectives"
        dragging={draggingId === "objectives"}
        onDragStart={onPointerDown}
        icon={<IconTarget />}
        title="Цели"
        count={activeGoals}
      >
        <form
          className="obj-add"
          onSubmit={(e) => {
            e.preventDefault();
            void addObjective();
          }}
        >
          <input
            className="rename-input"
            value={newTitle}
            onChange={(e) => setNewTitle(e.target.value)}
            placeholder="Новая цель…"
          />
          <textarea
            className="rename-input obj-steps-input"
            value={newSteps}
            onChange={(e) => setNewSteps(e.target.value)}
            placeholder="Шаги, по одному в строке (необязательно)"
            rows={2}
          />
          <button className="ghost-btn small" type="submit" disabled={!newTitle.trim()}>
            Добавить
          </button>
        </form>
        {data.objectives.length === 0 && (
          <div className="today-empty">Целей пока нет.</div>
        )}
        {data.objectives.map((o) => {
          const pct = o.totalSteps ? Math.round((o.doneSteps / o.totalSteps) * 100) : 0;
          return (
            <div className="obj-row" key={`g-${o.id}`}>
              <div
                className="obj-head"
                role="button"
                tabIndex={0}
                title={o.description ?? undefined}
                onClick={() => toggleObjective(o.id)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") toggleObjective(o.id);
                }}
              >
                <span className="t">{o.title}</span>
                <span className={`badge st-${o.status}`}>
                  {OBJECTIVE_STATUS_LABEL[o.status] ?? o.status}
                </span>
                {o.totalSteps > 0 && (
                  <span className="obj-count">
                    {o.doneSteps}/{o.totalSteps}
                  </span>
                )}
              </div>
              {o.totalSteps > 0 && (
                <div className="obj-bar">
                  <i style={{ width: `${pct}%` }} />
                </div>
              )}
              {expanded[o.id] && o.steps.length > 0 && (
                <div className="obj-steps">
                  {o.steps.map((s) => (
                    <label className="obj-step" key={s.id}>
                      <input
                        type="checkbox"
                        checked={s.done}
                        onChange={(e) => void toggleStep(o.id, s.id, e.target.checked)}
                      />
                      <span className={s.done ? "strikethrough" : undefined}>
                        {s.text}
                      </span>
                    </label>
                  ))}
                </div>
              )}
              <div className="obj-actions">
                {o.status !== "done" && (
                  <button
                    className="mini-btn"
                    type="button"
                    onClick={() => void setObjectiveStatus(o.id, "done")}
                  >
                    Готово
                  </button>
                )}
                {o.status === "active" && (
                  <button
                    className="mini-btn"
                    type="button"
                    onClick={() => void setObjectiveStatus(o.id, "paused")}
                  >
                    Пауза
                  </button>
                )}
                {o.status === "paused" && (
                  <button
                    className="mini-btn"
                    type="button"
                    onClick={() => void setObjectiveStatus(o.id, "active")}
                  >
                    Продолжить
                  </button>
                )}
              </div>
            </div>
          );
        })}
      </Card>
    ),
    notes: (
      <Card
        key="notes"
        cardId="notes"
        dragging={draggingId === "notes"}
        onDragStart={onPointerDown}
        icon={<IconNote />}
        title="Свежие заметки"
        action={{ label: "все", onClick: () => onTab("notes") }}
      >
        {data.notes.length === 0 && (
          <div className="today-empty">Заметок пока нет.</div>
        )}
        {data.notes.map((n) => (
          <div
            className="today-row"
            key={`n-${n.relPath}`}
            role="button"
            tabIndex={0}
            onClick={() => openNote(n.relPath)}
            onKeyDown={(e) => {
              if (e.key === "Enter") openNote(n.relPath);
            }}
          >
            <span className="today-row-title">{n.name}</span>
            <span className="today-row-meta">
              <span className="today-row-date">{formatModified(n.modified)}</span>
            </span>
          </div>
        ))}
      </Card>
    ),
    chats: (
      <Card
        key="chats"
        cardId="chats"
        dragging={draggingId === "chats"}
        onDragStart={onPointerDown}
        icon={<IconChat />}
        title="Последние чаты"
        action={{ label: "все", onClick: () => onTab("harness") }}
      >
        {data.sessions.length === 0 && (
          <div className="today-empty">Сессий пока нет.</div>
        )}
        {data.sessions.map((s) => (
          <div
            className="today-row"
            key={`c-${s.id}`}
            role="button"
            tabIndex={0}
            onClick={() => openSession(s.id)}
            onKeyDown={(e) => {
              if (e.key === "Enter") openSession(s.id);
            }}
          >
            <span className="today-row-title">{s.title ?? "Новая сессия"}</span>
            <span className="today-row-meta">
              <span className="today-row-date">{formatActivity(s.lastActiveAt)}</span>
            </span>
          </div>
        ))}
      </Card>
    ),
  };

  return (
    <main className="view active today-view">
      <div className="center">
        <header className="today-header">
          <div className="today-header-top">
            <div className="today-title-block">
              <h2 className="today-date">{todayLabel()}</h2>
              <p className="today-greeting">{data ? greetingLine(data) : "Загрузка…"}</p>
            </div>
            <button
              className="icon-btn small today-refresh"
              type="button"
              title="Обновить"
              onClick={() => void reload()}
            >
              <RefreshIcon />
            </button>
          </div>
        </header>

        <div className="center-body">
          {error && <div className="tree-error">{error}</div>}
          {!data && !error && <div className="empty-note">Загрузка…</div>}
          {cardNodes && (
            <div className="today-grid" ref={gridRef}>
              {order.map((id) => cardNodes[id])}
            </div>
          )}
        </div>
      </div>
    </main>
  );
}

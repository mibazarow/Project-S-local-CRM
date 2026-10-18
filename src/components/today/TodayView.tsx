import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import * as ipc from "../../api/ipc";
import { useApp } from "../../state/AppStore";
import type { FsEntry, ObjectiveDto, ProjectDto, ScheduleDto, SessionDto, TaskDto } from "../../api/types";
import { toMessage } from "../chat/chatShared";
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

const OBJECTIVE_STATUS_LABEL: Record<string, string> = {
  active: "в работе",
  paused: "пауза",
  done: "готово",
  error: "ошибка",
};

// sessions.last_active_at arrives as SQLite localtime "YYYY-MM-DD HH:MM:SS".
function formatActivity(value: string): string {
  const d = new Date(value.replace(" ", "T"));
  if (Number.isNaN(d.getTime())) return value;
  const time = d.toLocaleTimeString("ru-RU", { hour: "2-digit", minute: "2-digit" });
  if (d.toDateString() === new Date().toDateString()) return `сегодня ${time}`;
  return `${d.toLocaleDateString("ru-RU", { day: "numeric", month: "short" })} ${time}`;
}

function formatInstant(value: string): string {
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) return value;
  return d.toLocaleString("ru-RU", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });
}

function formatDue(dueDate: string): string {
  const d = new Date(`${dueDate}T00:00:00`);
  if (Number.isNaN(d.getTime())) return dueDate;
  return d.toLocaleDateString("ru-RU", { day: "numeric", month: "short" });
}

function formatModified(ts: number): string {
  const d = new Date(ts * 1000);
  if (Number.isNaN(d.getTime())) return "";
  const time = d.toLocaleTimeString("ru-RU", { hour: "2-digit", minute: "2-digit" });
  if (d.toDateString() === new Date().toDateString()) return `сегодня ${time}`;
  return `${d.toLocaleDateString("ru-RU", { day: "numeric", month: "short" })} ${time}`;
}

function todayLabel(): string {
  const s = new Date().toLocaleDateString("ru-RU", { weekday: "long", day: "numeric", month: "long" });
  return s.charAt(0).toUpperCase() + s.slice(1);
}

const RefreshIcon = () => (
  <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
    <path d="M21 12a9 9 0 1 1-2.64-6.36M21 3v6h-6" />
  </svg>
);

function Card({ title, count, children }: { title: string; count?: number; children: ReactNode }) {
  return (
    <section className="today-card">
      <div className="today-card-title">
        {title}
        {count != null && count > 0 && <span className="today-count">{count}</span>}
      </div>
      <div className="today-list">{children}</div>
    </section>
  );
}

export default function TodayView({ onTab }: Props) {
  const { actions } = useApp();
  const [data, setData] = useState<TodayData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Record<number, boolean>>({});
  const [newTitle, setNewTitle] = useState("");
  const [newSteps, setNewSteps] = useState("");

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
      projects.forEach((project, i) => {
        for (const task of tasksPer[i]) {
          if (task.status === "done" || !task.dueDate) continue;
          const due = new Date(`${task.dueDate}T00:00:00`).getTime();
          if (Number.isNaN(due)) continue;
          if (due < startOfDay.getTime()) overdue.push({ task, project });
          else if (due <= weekEnd) soon.push({ task, project });
        }
      });
      const byDue = (a: TaskWithProject, b: TaskWithProject) =>
        (a.task.dueDate ?? "").localeCompare(b.task.dueDate ?? "");
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

  const openTask = (projectId: number) => {
    void actions.openProject(projectId);
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

  return (
    <main className="view active today-view">
      <div className="center">
        <div className="center-head">
          <h2>{todayLabel()}</h2>
          {data && (
            <span className="today-head-counts">
              <span className={data.overdue.length ? "badge st-overdue" : "badge"}>
                просрочено {data.overdue.length}
              </span>
              <span className="badge">на неделе {data.soon.length}</span>
              <span className="badge">расписаний {data.schedules.length}</span>
            </span>
          )}
          <button className="icon-btn small" type="button" title="Обновить" onClick={() => void reload()}>
            <RefreshIcon />
          </button>
        </div>

        <div className="center-body">
          {error && <div className="tree-error">{error}</div>}
          {!data && !error && <div className="empty-note">Загрузка…</div>}
          {data && (
            <div className="today-grid">
              <Card title="Просрочено" count={data.overdue.length}>
                {data.overdue.length === 0 && (
                  <div className="empty-note">Просроченных задач нет.</div>
                )}
                {data.overdue.map(({ task, project }) => (
                  <div
                    className="today-item"
                    key={`o-${task.id}`}
                    role="button"
                    tabIndex={0}
                    onClick={() => openTask(project.id)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") openTask(project.id);
                    }}
                  >
                    <span className="t">{task.title}</span>
                    <span className="m">
                      <span className="proj">{project.name}</span>
                      <span className="red">{formatDue(task.dueDate!)}</span>
                    </span>
                  </div>
                ))}
              </Card>

              <Card title="Скоро (7 дней)" count={data.soon.length}>
                {data.soon.length === 0 && (
                  <div className="empty-note">На ближайшую неделю задач нет.</div>
                )}
                {data.soon.map(({ task, project }) => (
                  <div
                    className="today-item"
                    key={`s-${task.id}`}
                    role="button"
                    tabIndex={0}
                    onClick={() => openTask(project.id)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") openTask(project.id);
                    }}
                  >
                    <span className="t">{task.title}</span>
                    <span className="m">
                      <span className="proj">{project.name}</span>
                      <span>{formatDue(task.dueDate!)}</span>
                    </span>
                  </div>
                ))}
              </Card>

              <Card title="Расписания" count={data.schedules.length}>
                {data.schedules.length === 0 && (
                  <div className="empty-note">Активных расписаний нет. Создайте их во вкладке «Харнес».</div>
                )}
                {data.schedules.map((s) => (
                  <div
                    className="today-item"
                    key={`sc-${s.id}`}
                    role="button"
                    tabIndex={0}
                    title="Открыть чат расписания"
                    onClick={() => void openSchedule(s)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") void openSchedule(s);
                    }}
                  >
                    <span className="t">{s.title ?? s.spec}</span>
                    <span className="m">
                      <span>
                        {s.nextRunAt ? `след.: ${formatInstant(s.nextRunAt)}` : "без ближайшего запуска"}
                      </span>
                    </span>
                  </div>
                ))}
              </Card>

              <Card
                title="Цели"
                count={data.objectives.filter((o) => o.status !== "done").length}
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
                  <button className="mini-btn" type="submit" disabled={!newTitle.trim()}>
                    Добавить
                  </button>
                </form>
                {data.objectives.length === 0 && (
                  <div className="empty-note">Целей пока нет.</div>
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

              <Card title="Свежие заметки">
                {data.notes.length === 0 && <div className="empty-note">Заметок пока нет.</div>}
                {data.notes.map((n) => (
                  <div
                    className="today-item"
                    key={`n-${n.relPath}`}
                    role="button"
                    tabIndex={0}
                    onClick={() => openNote(n.relPath)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") openNote(n.relPath);
                    }}
                  >
                    <span className="t">{n.name}</span>
                    <span className="m">
                      <span>{formatModified(n.modified)}</span>
                    </span>
                  </div>
                ))}
              </Card>

              <Card title="Последние чаты">
                {data.sessions.length === 0 && <div className="empty-note">Сессий пока нет.</div>}
                {data.sessions.map((s) => (
                  <div
                    className="today-item"
                    key={`c-${s.id}`}
                    role="button"
                    tabIndex={0}
                    onClick={() => openSession(s.id)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") openSession(s.id);
                    }}
                  >
                    <span className="t">{s.title ?? "Новая сессия"}</span>
                    <span className="m">
                      <span>{formatActivity(s.lastActiveAt)}</span>
                    </span>
                  </div>
                ))}
              </Card>
            </div>
          )}
        </div>
      </div>
    </main>
  );
}

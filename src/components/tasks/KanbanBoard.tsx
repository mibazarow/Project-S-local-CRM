import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import * as ipc from "../../api/ipc";
import type { TaskDto, TaskPatch, TemplateSummary } from "../../api/types";
import { toMessage } from "../files/fileOps";
import RenameInput from "../files/RenameInput";
import { IconClose, IconPlus, IconSearch, IconTrash } from "../files/icons";
import TaskCard from "./TaskCard";
import { usePointerDrag } from "../../hooks/usePointerDrag";
import { useApp } from "../../state/AppStore";

const COLUMNS = [
  { status: "new", label: "Новые" },
  { status: "progress", label: "В работе" },
  { status: "done", label: "Готово" },
] as const;

const PRIORITY_OPTIONS = [
  { value: "low", label: "низкий" },
  { value: "normal", label: "обычный" },
  { value: "high", label: "высокий" },
  { value: "urgent", label: "срочный" },
];

interface Filters {
  query: string;
  priority: string;
  tag: string;
  overdueOnly: boolean;
  hideDone: boolean;
}

const EMPTY_FILTERS: Filters = {
  query: "",
  priority: "",
  tag: "",
  overdueOnly: false,
  hideDone: false,
};

const isDefaultFilters = (f: Filters) =>
  !f.query.trim() && !f.priority && !f.tag && !f.overdueOnly && !f.hideDone;

// Saved views: named filter sets per project in localStorage.
interface SavedView {
  name: string;
  filters: Filters;
}

const viewsKey = (projectId: number) => `project-s:taskViews:${projectId}`;

function readViews(projectId: number): SavedView[] {
  try {
    const raw = localStorage.getItem(viewsKey(projectId));
    const parsed: unknown = raw === null ? [] : JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (v): v is SavedView =>
        typeof v === "object" &&
        v !== null &&
        typeof (v as SavedView).name === "string" &&
        typeof (v as SavedView).filters === "object" &&
        (v as SavedView).filters !== null,
    );
  } catch {
    return [];
  }
}

function writeViews(projectId: number, views: SavedView[]): void {
  try {
    localStorage.setItem(viewsKey(projectId), JSON.stringify(views));
  } catch {
    // Storage unavailable: views simply won't persist across restarts.
  }
}

interface Props {
  projectId: number;
  tasksVersion: number;
  highlightTaskId: number | null;
}

export default function KanbanBoard({ projectId, tasksVersion, highlightTaskId }: Props) {
  const [tasks, setTasks] = useState<TaskDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [addingStatus, setAddingStatus] = useState<string | null>(null);
  const [filters, setFilters] = useState<Filters>(EMPTY_FILTERS);
  const [views, setViews] = useState<SavedView[]>([]);
  const [activeView, setActiveView] = useState("");
  const [taskTemplates, setTaskTemplates] = useState<TemplateSummary[]>([]);
  const boardRef = useRef<HTMLDivElement>(null);
  const { actions } = useApp();

  // «Обсудить»: a prebuilt question about the task for the project chat.
  const discussTask = (task: TaskDto) => {
    const detail = task.comment?.trim() || (task.dueDate ? `срок ${task.dueDate}` : "");
    actions.requestAskAgent(
      projectId,
      `Задача #${task.id} «${task.title}»${detail ? `: ${detail}` : ""}. Кратко: текущий статус и следующий шаг.`,
    );
  };

  // Task templates for the «Задача из шаблона» picker; a failed load hides it.
  useEffect(() => {
    let cancelled = false;
    void ipc
      .templatesList()
      .then((t) => !cancelled && setTaskTemplates(t.tasks))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  // Per-project filter/view state: reset and reload on project switch.
  useEffect(() => {
    setFilters(EMPTY_FILTERS);
    setActiveView("");
    setViews(readViews(projectId));
  }, [projectId]);

  const patchFilters = (patch: Partial<Filters>) => {
    setFilters((prev) => ({ ...prev, ...patch }));
    setActiveView("");
  };

  const allTags = useMemo(() => {
    const set = new Set<string>((tasks ?? []).flatMap((t) => t.tags));
    if (filters.tag) set.add(filters.tag);
    return Array.from(set).sort((a, b) => a.localeCompare(b, "ru"));
  }, [tasks, filters.tag]);

  const visible = useMemo(() => {
    const q = filters.query.trim().toLowerCase();
    return (tasks ?? []).filter((t) => {
      if (q && !t.title.toLowerCase().includes(q)) return false;
      if (filters.priority && t.priority !== filters.priority) return false;
      if (filters.tag && !t.tags.includes(filters.tag)) return false;
      if (filters.overdueOnly && !t.overdue) return false;
      if (filters.hideDone && t.status === "done") return false;
      return true;
    });
  }, [tasks, filters]);

  const byStatus = useMemo(() => {
    const map: Record<string, TaskDto[]> = { new: [], progress: [], done: [] };
    for (const t of visible) map[t.status]?.push(t);
    return map;
  }, [visible]);

  const { draggingId, dropTarget, onPointerDown } = usePointerDrag({
    // Target column = the one whose bounds contain the pointer; insertion
    // index = pointer Y vs the midlines of that column's non-dragged cards.
    resolveTarget: (clientX, clientY, id) => {
      const cols = Array.from(
        boardRef.current?.querySelectorAll<HTMLElement>(".task-col[data-status]") ?? [],
      );
      for (const col of cols) {
        const bounds = col.getBoundingClientRect();
        if (
          clientX < bounds.left ||
          clientX > bounds.right ||
          clientY < bounds.top ||
          clientY > bounds.bottom
        ) {
          continue;
        }
        const cards = Array.from(col.querySelectorAll<HTMLElement>(".task-card[data-id]")).filter(
          (el) => Number(el.dataset.id) !== id,
        );
        let index = cards.length;
        for (let i = 0; i < cards.length; i++) {
          const rect = cards[i].getBoundingClientRect();
          if (clientY < rect.top + rect.height / 2) {
            index = i;
            break;
          }
        }
        return { container: col.dataset.status ?? "", index };
      }
      return null;
    },
    // Midpoint between neighbors, first-1 for the top, last+1 for the end.
    onDrop: (id, target) => {
      const others = (byStatus[target.container] ?? []).filter((t) => t.id !== id);
      let position: number;
      if (others.length === 0) position = 1;
      else if (target.index === 0) position = others[0].position - 1;
      else if (target.index >= others.length) position = others[others.length - 1].position + 1;
      else position = (others[target.index - 1].position + others[target.index].position) / 2;
      ipc
        .taskMove(id, target.container, position)
        .then((dto) => {
          setError(null);
          upsert(dto);
        })
        .catch((e) => setError(toMessage(e)));
    },
  });

  // Reload on every tasks-changed bump for this project (agent or user
  // mutation). Debounced so a burst of tool calls lands as one fetch, and
  // the cancelled flag keeps a late response from overwriting a newer one.
  // While the user is mid-drag the reload is deferred: an agent move must not
  // clobber the in-progress drag; draggingId returning to null re-runs this
  // effect and reconciles then.
  useEffect(() => {
    if (draggingId !== null) return;
    let cancelled = false;
    const timer = setTimeout(() => {
      ipc
        .tasksList(projectId)
        .then((list) => {
          if (!cancelled) {
            setTasks(list);
            setError(null);
          }
        })
        .catch((e) => {
          if (!cancelled) setError(toMessage(e));
        });
    }, 200);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [projectId, tasksVersion, draggingId]);

  // Apply the returned DTO locally for immediate feel; tasks-changed refetch reconciles.
  const upsert = (dto: TaskDto) =>
    setTasks((prev) => {
      if (!prev) return prev;
      const i = prev.findIndex((t) => t.id === dto.id);
      if (i === -1) return [...prev, dto];
      const next = [...prev];
      next[i] = dto;
      return next;
    });

  const handleCreate = (status: string, title: string) => {
    setAddingStatus(null);
    ipc
      .taskCreate(projectId, title, status)
      .then((dto) => {
        setError(null);
        upsert(dto);
      })
      .catch((e) => setError(toMessage(e)));
  };

  const handleTemplateTask = (templateName: string) => {
    if (!templateName) return;
    ipc
      .templateCreateTask(projectId, templateName)
      .then((dto) => {
        setError(null);
        upsert(dto);
      })
      .catch((e) => setError(toMessage(e)));
  };

  const handlePatch = (id: number, patch: TaskPatch) => {
    ipc
      .taskUpdate(id, patch)
      .then((dto) => {
        setError(null);
        upsert(dto);
      })
      .catch((e) => setError(toMessage(e)));
  };

  const handleDelete = (task: TaskDto) => {
    if (!window.confirm(`Удалить задачу «${task.title}»?`)) return;
    ipc
      .taskDelete(task.id)
      .then(() => {
        setError(null);
        setTasks((prev) => prev?.filter((t) => t.id !== task.id) ?? prev);
      })
      .catch((e) => setError(toMessage(e)));
  };

  const handleViewSelect = (name: string) => {
    if (name === "") {
      setFilters(EMPTY_FILTERS);
      setActiveView("");
      return;
    }
    const view = views.find((v) => v.name === name);
    if (!view) return;
    setFilters({ ...EMPTY_FILTERS, ...view.filters });
    setActiveView(name);
  };

  const handleSaveView = () => {
    const name = window.prompt("Название вида фильтров:");
    if (!name?.trim()) return;
    const trimmed = name.trim();
    const next = [...views.filter((v) => v.name !== trimmed), { name: trimmed, filters }];
    setViews(next);
    writeViews(projectId, next);
    setActiveView(trimmed);
  };

  const handleDeleteView = () => {
    const next = views.filter((v) => v.name !== activeView);
    setViews(next);
    writeViews(projectId, next);
    setActiveView("");
  };

  const renderCard = (t: TaskDto, dragging: boolean) => (
    <TaskCard
      key={t.id}
      task={t}
      dragging={dragging}
      highlight={highlightTaskId === t.id}
      onPointerDown={(e) => onPointerDown(e, t.id)}
      onPatch={(patch) => handlePatch(t.id, patch)}
      onDelete={() => handleDelete(t)}
      onDiscuss={() => discussTask(t)}
    />
  );

  return (
    <div className="center-body">
      {error && <div className="tree-error">{error}</div>}
      <div className="task-filters">
        <div className="tf-search">
          <IconSearch />
          <input
            type="text"
            placeholder="Поиск по названию…"
            value={filters.query}
            onChange={(e) => patchFilters({ query: e.target.value })}
          />
          {filters.query && (
            <button
              className="mini-btn"
              title="Очистить поиск"
              onClick={() => patchFilters({ query: "" })}
            >
              <IconClose />
            </button>
          )}
        </div>
        <select
          className="tf-select"
          value={filters.priority}
          onChange={(e) => patchFilters({ priority: e.target.value })}
        >
          <option value="">Приоритет: все</option>
          {PRIORITY_OPTIONS.map((p) => (
            <option key={p.value} value={p.value}>
              {p.label}
            </option>
          ))}
        </select>
        <select
          className="tf-select"
          value={filters.tag}
          onChange={(e) => patchFilters({ tag: e.target.value })}
        >
          <option value="">Теги: все</option>
          {allTags.map((tag) => (
            <option key={tag} value={tag}>
              {tag}
            </option>
          ))}
        </select>
        {taskTemplates.length > 0 && (
          <select
            className="tf-select"
            value=""
            title="Создать задачу из шаблона"
            onChange={(e) => handleTemplateTask(e.target.value)}
          >
            <option value="">Задача из шаблона…</option>
            {taskTemplates.map((t) => (
              <option key={t.name} value={t.name}>
                {t.name}
              </option>
            ))}
          </select>
        )}
        <label className="tf-check">
          <input
            type="checkbox"
            checked={filters.overdueOnly}
            onChange={(e) => patchFilters({ overdueOnly: e.target.checked })}
          />
          только просроченные
        </label>
        <label className="tf-check">
          <input
            type="checkbox"
            checked={filters.hideDone}
            onChange={(e) => patchFilters({ hideDone: e.target.checked })}
          />
          скрыть готовые
        </label>
        <div className="tf-views">
          <select
            className="tf-select"
            value={activeView || (isDefaultFilters(filters) ? "" : "custom")}
            onChange={(e) => handleViewSelect(e.target.value)}
          >
            <option value="">Вид: по умолчанию</option>
            {views.map((v) => (
              <option key={v.name} value={v.name}>
                {v.name}
              </option>
            ))}
            <option value="custom" disabled>
              свой набор
            </option>
          </select>
          <button
            className="mini-btn"
            title="Сохранить текущие фильтры как вид"
            onClick={handleSaveView}
          >
            <IconPlus />
          </button>
          {activeView && (
            <button className="mini-btn danger" title="Удалить вид" onClick={handleDeleteView}>
              <IconTrash />
            </button>
          )}
        </div>
      </div>
      {tasks && (
        <div className="task-board" ref={boardRef}>
          {COLUMNS.map((col) => {
            const cards = byStatus[col.status];
            const over = dropTarget !== null && dropTarget.container === col.status;
            const dropIndex = over ? dropTarget.index : null;
            const nodes: ReactNode[] = [];
            let visibleIndex = 0;
            for (const t of cards) {
              if (t.id === draggingId) {
                nodes.push(renderCard(t, true));
                continue;
              }
              if (dropIndex !== null && dropIndex === visibleIndex) {
                nodes.push(<div key="drop-indicator" className="drop-indicator" />);
              }
              nodes.push(renderCard(t, false));
              visibleIndex++;
            }
            if (dropIndex !== null && dropIndex === visibleIndex) {
              nodes.push(<div key="drop-indicator" className="drop-indicator" />);
            }
            return (
              <div
                key={col.status}
                data-status={col.status}
                className={over ? "task-col drop-over" : "task-col"}
              >
                <div className="task-col-head">
                  {col.label} <span className="count">{cards.length}</span>
                  <button
                    className="mini-btn task-add-btn"
                    title="Добавить задачу"
                    onClick={() => setAddingStatus(col.status)}
                  >
                    <IconPlus />
                  </button>
                </div>
                <div className="task-list">
                  {addingStatus === col.status && (
                    <RenameInput
                      value=""
                      placeholder="Новая задача…"
                      onCommit={(title) => handleCreate(col.status, title)}
                      onCancel={() => setAddingStatus(null)}
                    />
                  )}
                  {nodes}
                  {tasks && cards.length === 0 && addingStatus !== col.status && (
                    <div className="empty-note">
                      {isDefaultFilters(filters) ? "Пусто" : "Нет подходящих"}
                    </div>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

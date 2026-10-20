import {
  Fragment,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import type { TaskDto, TaskPatch } from "../../api/types";
import { dueMs, formatDue, formatSpent } from "../../shared/format";
import {
  IconAskAgent,
  IconComment,
  IconPencil,
  IconPlus,
  IconTrash,
} from "../files/icons";
import { CommentInput, TaskEditor } from "./TaskCard";
import { blankTask, positionForDrop } from "./taskHelpers";
import { usePointerDrag, type DropTarget } from "../../hooks/usePointerDrag";

const STATUSES = ["new", "progress", "done"] as const;

// Drag group key -> backend status it commits to. The «Сейчас» block is the
// active subset of progress, so dropping there also lands in progress.
const GROUP_STATUS: Record<string, string> = {
  now: "progress",
  news: "new",
  progress: "progress",
  done: "done",
};

interface Props {
  tasks: TaskDto[];
  columns: string[];
  overdueOnly: boolean;
  projectId?: number;
  onPatch?: (id: number, patch: TaskPatch) => void;
  onDelete?: (task: TaskDto) => void;
  onDiscuss?: (task: TaskDto) => void;
  onCreate?: (status: string, patch: TaskPatch) => void;
  onMove?: (id: number, status: string, position: number) => void;
}

// Within a group: priority first, then nearest deadline, then position, then id.
const sortTasks = (a: TaskDto, b: TaskDto): number => {
  if (a.isPriority !== b.isPriority) return a.isPriority ? -1 : 1;
  const da = a.dueDate ? dueMs(a.dueDate, a.dueTime) : Number.POSITIVE_INFINITY;
  const db = b.dueDate ? dueMs(b.dueDate, b.dueTime) : Number.POSITIVE_INFINITY;
  if (da !== db) return da - db;
  if (a.position !== b.position) return a.position - b.position;
  return a.id - b.id;
};

interface RowHandlers {
  onStart: (task: TaskDto) => void;
  onComplete: (task: TaskDto) => void;
  onReopen: (task: TaskDto) => void;
  onEdit: (task: TaskDto) => void;
  onComment: (task: TaskDto) => void;
  onAgent?: (task: TaskDto) => void;
  onDelete?: (task: TaskDto) => void;
  onPointerDown: (e: ReactPointerEvent<HTMLElement>, id: number) => void;
}

function TaskRow({
  task,
  statusLabel,
  active,
  dragging,
  h,
}: {
  task: TaskDto;
  statusLabel: string;
  active?: boolean;
  dragging?: boolean;
  h: RowHandlers;
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
  if (dragging) classes.push("dragging");

  const stop = (fn: () => void) => (e: ReactMouseEvent) => {
    e.stopPropagation();
    fn();
  };

  return (
    <div
      className={classes.join(" ")}
      data-id={task.id}
      onPointerDown={(e) => h.onPointerDown(e, task.id)}
    >
      <span className={`task-list-dot ${task.status}`} />
      <span className="task-list-status">{statusLabel}</span>
      <span
        className="task-list-title"
        title={task.title}
        onClick={stop(() => h.onEdit(task))}
      >
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
      <span className="task-list-actions">
        {task.status === "new" && (
          <button className="mini-btn" title="Начать" onClick={stop(() => h.onStart(task))}>
            ▶
          </button>
        )}
        {task.status !== "done" && (
          <button className="mini-btn" title="Готово" onClick={stop(() => h.onComplete(task))}>
            ✓
          </button>
        )}
        {task.status === "done" && (
          <button className="mini-btn" title="Вернуть в новые" onClick={stop(() => h.onReopen(task))}>
            ↺
          </button>
        )}
        {h.onAgent && (
          <button className="mini-btn" title="Обсудить с агентом" onClick={stop(() => h.onAgent!(task))}>
            <IconAskAgent />
          </button>
        )}
        <button className="mini-btn" title="Редактировать" onClick={stop(() => h.onEdit(task))}>
          <IconPencil />
        </button>
        <button className="mini-btn" title="Комментарий" onClick={stop(() => h.onComment(task))}>
          <IconComment />
        </button>
        {h.onDelete && (
          <button className="mini-btn danger" title="Удалить" onClick={stop(() => h.onDelete!(task))}>
            <IconTrash />
          </button>
        )}
      </span>
    </div>
  );
}

export default function TaskListView({
  tasks,
  columns,
  overdueOnly,
  projectId = 0,
  onPatch,
  onDelete,
  onDiscuss,
  onCreate,
  onMove,
}: Props) {
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({ done: true });
  const [expandedId, setExpandedId] = useState<number | null>(null);
  const [commentId, setCommentId] = useState<number | null>(null);
  const [adding, setAdding] = useState(false);
  const [newStatus, setNewStatus] = useState<string>("new");
  const listRef = useRef<HTMLDivElement>(null);

  const groups = useMemo(() => {
    const isNow = (t: TaskDto) => t.status === "progress" && t.startedAt !== null;
    const now = tasks.filter(isNow);
    const rest = tasks.filter((t) => !isNow(t));
    const overdue = rest.filter((t) => t.overdue);
    const fresh = rest.filter((t) => !t.overdue);
    const group = (status: string) => fresh.filter((t) => t.status === status).sort(sortTasks);
    return {
      now: now.sort(sortTasks),
      overdue: overdue.sort(sortTasks),
      news: group("new"),
      progress: group("progress"),
      done: group("done"),
    };
  }, [tasks]);

  // Latest groups for the drag onDrop closure (position from displayed order).
  const groupsRef = useRef(groups);
  groupsRef.current = groups;

  const { draggingId, dropTarget, onPointerDown, consumeSuppressedClick } = usePointerDrag({
    resolveTarget: (clientX, clientY, id): DropTarget<number> | null => {
      const containers = Array.from(
        listRef.current?.querySelectorAll<HTMLElement>(".task-list-rows[data-group]") ?? [],
      );
      for (const c of containers) {
        const bounds = c.getBoundingClientRect();
        if (
          clientX < bounds.left ||
          clientX > bounds.right ||
          clientY < bounds.top ||
          clientY > bounds.bottom
        ) {
          continue;
        }
        const rows = Array.from(c.querySelectorAll<HTMLElement>(".task-list-row[data-id]")).filter(
          (el) => Number(el.dataset.id) !== id,
        );
        let index = rows.length;
        for (let i = 0; i < rows.length; i++) {
          const rect = rows[i].getBoundingClientRect();
          if (clientY < rect.top + rect.height / 2) {
            index = i;
            break;
          }
        }
        return { container: c.dataset.group ?? "", index, draggingId: id };
      }
      return null;
    },
    onDrop: (id, target) => {
      const status = GROUP_STATUS[target.container];
      if (!status) return;
      const groupList = groupsRef.current[target.container as keyof typeof groupsRef.current] ?? [];
      const others = (groupList as TaskDto[]).filter((t) => t.id !== id);
      const position = positionForDrop(others, target.index);
      onMove?.(id, status, position);
    },
  });

  const statusLabel = (status: string) =>
    columns[STATUSES.indexOf(status as (typeof STATUSES)[number])] ?? status;

  const appendPosition = (task: TaskDto, status: string): number => {
    const key = status === "new" ? "news" : status === "done" ? "done" : "progress";
    const others = (groupsRef.current[key] ?? []).filter((t) => t.id !== task.id);
    return positionForDrop(others, others.length);
  };

  const handlers: RowHandlers = {
    onStart: (task) => onMove?.(task.id, "progress", appendPosition(task, "progress")),
    onComplete: (task) => onMove?.(task.id, "done", appendPosition(task, "done")),
    onReopen: (task) => onMove?.(task.id, "new", appendPosition(task, "new")),
    onEdit: (task) => {
      if (consumeSuppressedClick()) return;
      setCommentId(null);
      setExpandedId((prev) => (prev === task.id ? null : task.id));
    },
    onComment: (task) => {
      setExpandedId(null);
      setCommentId((prev) => (prev === task.id ? null : task.id));
    },
    onAgent: onDiscuss,
    onDelete: onDelete,
    onPointerDown,
  };

  const commitComment = (task: TaskDto, raw: string) => {
    setCommentId(null);
    const next = raw.trim();
    if (next === (task.comment ?? "")) return;
    onPatch?.(task.id, next ? { comment: next } : { comment: null });
  };

  if (tasks.length === 0) {
    return (
      <div className="task-list-view" ref={listRef}>
        {onCreate && (
          <div className="task-list-head">
            <select
              className="rename-input task-list-new-status"
              value={newStatus}
              onChange={(e) => setNewStatus(e.target.value)}
            >
              {STATUSES.map((s, i) => (
                <option key={s} value={s}>
                  {columns[i] ?? s}
                </option>
              ))}
            </select>
            <button className="mini-btn task-list-add-btn" title="Добавить задачу" onClick={() => setAdding(true)}>
              <IconPlus />
            </button>
          </div>
        )}
        {adding && (
          <div className="task-add-form">
            <TaskEditor
              task={blankTask(projectId, newStatus)}
              onSave={(patch) => {
                onCreate?.(newStatus, patch);
                setAdding(false);
              }}
              onCancel={() => setAdding(false)}
            />
          </div>
        )}
        <div className="task-list-empty">
          {overdueOnly ? "Нет просроченных задач" : "Пока нет задач"}
        </div>
      </div>
    );
  }

  const renderRows = (groupKey: string, list: TaskDto[]) => {
    const over = dropTarget !== null && dropTarget.container === groupKey;
    const dropIndex = over ? dropTarget.index : null;
    const nodes: ReactNode[] = [];
    let visibleIndex = 0;
    for (const t of list) {
      if (t.id === draggingId) {
        nodes.push(
          <TaskRow
            key={t.id}
            task={t}
            statusLabel={statusLabel(t.status)}
            active={groupKey === "now"}
            dragging
            h={handlers}
          />,
        );
        continue;
      }
      if (dropIndex !== null && dropIndex === visibleIndex) {
        nodes.push(<div key="drop-indicator" className="drop-indicator" />);
      }
      nodes.push(
        <Fragment key={t.id}>
          <TaskRow
            task={t}
            statusLabel={statusLabel(t.status)}
            active={groupKey === "now"}
            h={handlers}
          />
          {expandedId === t.id && (
            <TaskEditor
              task={t}
              onSave={(patch) => {
                setExpandedId(null);
                onPatch?.(t.id, patch);
              }}
              onCancel={() => setExpandedId(null)}
            />
          )}
          {commentId === t.id && (
            <CommentInput
              value={t.comment ?? ""}
              onCommit={(raw) => commitComment(t, raw)}
              onCancel={() => setCommentId(null)}
            />
          )}
        </Fragment>,
      );
      visibleIndex++;
    }
    if (dropIndex !== null && dropIndex === visibleIndex) {
      nodes.push(<div key="drop-indicator" className="drop-indicator" />);
    }
    return nodes;
  };

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
          <div className="task-list-rows" data-group={key}>
            {renderRows(key, list)}
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
    <div className="task-list-view" ref={listRef}>
      {onCreate && (
        <div className="task-list-head">
          <select
            className="rename-input task-list-new-status"
            value={newStatus}
            onChange={(e) => setNewStatus(e.target.value)}
          >
            {STATUSES.map((s, i) => (
              <option key={s} value={s}>
                {columns[i] ?? s}
              </option>
            ))}
          </select>
          <button className="mini-btn task-list-add-btn" title="Добавить задачу" onClick={() => setAdding(true)}>
            <IconPlus />
          </button>
        </div>
      )}
      {adding && (
        <div className="task-add-form">
          <TaskEditor
            task={blankTask(projectId, newStatus)}
            onSave={(patch) => {
              onCreate?.(newStatus, patch);
              setAdding(false);
            }}
            onCancel={() => setAdding(false)}
          />
        </div>
      )}
      {groups.now.length > 0 && (
        <section className="task-list-now">
          <div className="task-list-now-head">
            <span className="task-list-group-label">Сейчас</span>
            <span className="count">{groups.now.length}</span>
          </div>
          <div className="task-list-rows" data-group="now">
            {renderRows("now", groups.now)}
          </div>
        </section>
      )}
      {allDone && <div className="task-list-empty">Все задачи выполнены</div>}
      {groups.overdue.length > 0 && (
        <section className="task-list-group">
          <button
            type="button"
            className={collapsed.overdue ? "task-list-group-head collapsed" : "task-list-group-head"}
            onClick={() => setCollapsed((prev) => ({ ...prev, overdue: !prev.overdue }))}
          >
            <span className="task-list-caret" />
            <span className="task-list-group-label">Просрочено</span>
            <span className="count">{groups.overdue.length}</span>
          </button>
          {!collapsed.overdue && (
            <div className="task-list-rows">{renderRows("overdue", groups.overdue)}</div>
          )}
        </section>
      )}
      {renderGroup("news", statusLabel("new"), groups.news)}
      {renderGroup("progress", statusLabel("progress"), groups.progress)}
      {renderGroup("done", statusLabel("done"), groups.done)}
    </div>
  );
}

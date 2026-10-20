import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import * as ipc from "../../api/ipc";
import type { ProjectDto, TaskDto, TaskPatch } from "../../api/types";
import { toMessage } from "../files/fileOps";
import { confirmRemove } from "../../shared/confirm";
import RenameInput from "../files/RenameInput";
import { IconPencil, IconPlus } from "../files/icons";
import TaskCard, { TaskEditor } from "./TaskCard";
import TaskListView from "./TaskListView";
import { usePointerDrag, type DropTarget } from "../../hooks/usePointerDrag";
import { useAppActions } from "../../state/AppStore";

// Column identities are fixed backend statuses; only the labels are
// per-project (ProjectDto.taskColumns).
const STATUSES = ["new", "progress", "done"] as const;
const DEFAULT_COLUMNS = ["Новые", "В работе", "Готово"];

const blankTask = (projectId: number, status: string): TaskDto => ({
  id: -1,
  projectId,
  title: "",
  status,
  comment: null,
  dueDate: null,
  dueTime: "07:00",
  remindAt: null,
  position: 0,
  createdAt: "",
  closedAt: null,
  overdue: false,
  isPriority: false,
  checklist: [],
  recurrence: "none",
  spentMs: 0,
  startedAt: null,
});

interface Props {
  project: ProjectDto;
  tasksVersion: number;
  highlightTaskId: number | null;
  overdueOnly?: boolean;
  view?: "kanban" | "list";
}

export default function KanbanBoard({
  project,
  tasksVersion,
  highlightTaskId,
  overdueOnly = false,
  view = "kanban",
}: Props) {
  const projectId = project.id;
  const [tasks, setTasks] = useState<TaskDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [addingStatus, setAddingStatus] = useState<string | null>(null);
  const [renamingIndex, setRenamingIndex] = useState<number | null>(null);
  const boardRef = useRef<HTMLDivElement>(null);
  const actions = useAppActions();

  // «Обсудить»: a prebuilt question about the task for the project chat.
  const discussTask = (task: TaskDto) => {
    const detail = task.comment?.trim() || (task.dueDate ? `срок ${task.dueDate} ${task.dueTime}` : "");
    actions.requestAskAgent(
      projectId,
      `Задача #${task.id} «${task.title}»${detail ? `: ${detail}` : ""}. Кратко: текущий статус и следующий шаг.`,
    );
  };

  const columns =
    project.taskColumns?.length === 3 ? project.taskColumns : DEFAULT_COLUMNS;

  const commitColumnRename = (index: number, label: string) => {
    setRenamingIndex(null);
    if (label === columns[index]) return;
    const next = columns.map((c, j) => (j === index ? label : c));
    // The backend emits "projects-changed"; AppStore refreshes the project
    // list, so the new labels arrive back through the `project` prop.
    ipc
      .setTaskColumns(projectId, next)
      .then(() => setError(null))
      .catch((e) => setError(toMessage(e)));
  };

  const visible = useMemo(() => {
    return (tasks ?? []).filter((t) => !overdueOnly || t.overdue);
  }, [tasks, overdueOnly]);

  const byStatus = useMemo(() => {
    const map: Record<string, TaskDto[]> = { new: [], progress: [], done: [] };
    for (const t of visible) map[t.status]?.push(t);
    return map;
  }, [visible]);

  const { draggingId, dropTarget, onPointerDown } = usePointerDrag({
    // Target column = the one whose bounds contain the pointer; insertion
    // index = pointer Y vs the midlines of that column's non-dragged cards.
    resolveTarget: (clientX, clientY, id): DropTarget<number> | null => {
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
        return { container: col.dataset.status ?? "", index, draggingId: id };
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

  // Focus view override: while a task is focused (highlighted) it is raised to
  // the top of its own column for visibility. This is display-only — the real
  // fractional `position` values and the DB order are untouched, and the drop
  // math above keeps reading `byStatus` (real order). The override is suspended
  // while dragging so the DOM order matches `byStatus` and a drop after focus
  // still computes positions from the real neighbors.
  const displayByStatus = useMemo(() => {
    if (highlightTaskId == null || draggingId !== null) return byStatus;
    const map: Record<string, TaskDto[]> = { ...byStatus };
    for (const status of Object.keys(map)) {
      const arr = map[status];
      const idx = arr.findIndex((t) => t.id === highlightTaskId);
      if (idx > 0) map[status] = [arr[idx], ...arr.slice(0, idx), ...arr.slice(idx + 1)];
    }
    return map;
  }, [byStatus, highlightTaskId, draggingId]);

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

  const handleCreate = (status: string, patch: TaskPatch) => {
    ipc
      .taskCreate(
        projectId,
        patch.title ?? "",
        status,
        patch.dueDate,
        patch.dueTime,
        patch.isPriority,
        patch.checklist,
      )
      .then((created) => {
        setAddingStatus(null);
        setError(null);
        if (patch.comment != null || patch.remindAt != null) {
          ipc
            .taskUpdate(created.id, {
              comment: patch.comment,
              remindAt: patch.remindAt,
            })
            .then((dto) => upsert(dto))
            .catch((e) => setError(toMessage(e)));
        } else {
          upsert(created);
        }
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

  const handleDelete = async (task: TaskDto) => {
    if (!(await confirmRemove({ title: `Удалить задачу «${task.title}»?` }))) return;
    ipc
      .taskDelete(task.id)
      .then(() => {
        setError(null);
        setTasks((prev) => prev?.filter((t) => t.id !== task.id) ?? prev);
      })
      .catch((e) => setError(toMessage(e)));
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
      {view === "list" ? (
        tasks && (
          <TaskListView
            tasks={visible}
            columns={columns}
            overdueOnly={overdueOnly}
          />
        )
      ) : (
        tasks && (
        <div className="task-board" ref={boardRef}>
          {STATUSES.map((status, i) => {
            const cards = displayByStatus[status];
            const over = dropTarget !== null && dropTarget.container === status;
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
                key={status}
                data-status={status}
                className={over ? "task-col drop-over" : "task-col"}
              >
                <div className="task-col-head">
                  {renamingIndex === i ? (
                    <RenameInput
                      value={columns[i]}
                      onCommit={(name) => commitColumnRename(i, name)}
                      onCancel={() => setRenamingIndex(null)}
                    />
                  ) : (
                    <span
                      className="task-col-label"
                      title="Двойной клик — переименовать колонку"
                      onDoubleClick={() => setRenamingIndex(i)}
                    >
                      {columns[i]}
                    </span>
                  )}
                  <span className="count">{cards.length}</span>
                  {renamingIndex !== i && (
                    <button
                      className="mini-btn task-col-rename-btn"
                      title="Переименовать колонку"
                      onClick={() => setRenamingIndex(i)}
                    >
                      <IconPencil />
                    </button>
                  )}
                  <button
                    className="mini-btn task-add-btn"
                    title="Добавить задачу"
                    onClick={() => setAddingStatus(status)}
                  >
                    <IconPlus />
                  </button>
                </div>
                <div className="task-list">
                  {addingStatus === status && (
                    <div className="task-add-form">
                      <TaskEditor
                        task={blankTask(projectId, status)}
                        onSave={(patch) => handleCreate(status, patch)}
                        onCancel={() => setAddingStatus(null)}
                      />
                    </div>
                  )}
                  {nodes}
                  {tasks && cards.length === 0 && addingStatus !== status && (
                    <div className="empty-note">
                      {overdueOnly ? "Нет подходящих" : "Пусто"}
                    </div>
                  )}
                </div>
              </div>
            );
          })}
        </div>
        )
      )}
    </div>
  );
}

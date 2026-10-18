import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import * as ipc from "../../api/ipc";
import type { TaskDto, TaskPatch } from "../../api/types";
import { toMessage } from "../files/fileOps";
import RenameInput from "../files/RenameInput";
import { IconPlus } from "../files/icons";
import TaskCard from "./TaskCard";
import { usePointerDrag } from "../../hooks/usePointerDrag";

const COLUMNS = [
  { status: "new", label: "Новые" },
  { status: "progress", label: "В работе" },
  { status: "done", label: "Готово" },
] as const;

interface Props {
  projectId: number;
  tasksVersion: number;
  highlightTaskId: number | null;
}

export default function KanbanBoard({ projectId, tasksVersion, highlightTaskId }: Props) {
  const [tasks, setTasks] = useState<TaskDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [addingStatus, setAddingStatus] = useState<string | null>(null);
  const boardRef = useRef<HTMLDivElement>(null);

  const byStatus = useMemo(() => {
    const map: Record<string, TaskDto[]> = { new: [], progress: [], done: [] };
    for (const t of tasks ?? []) map[t.status]?.push(t);
    return map;
  }, [tasks]);

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

  const renderCard = (t: TaskDto, dragging: boolean) => (
    <TaskCard
      key={t.id}
      task={t}
      dragging={dragging}
      highlight={highlightTaskId === t.id}
      onPointerDown={(e) => onPointerDown(e, t.id)}
      onPatch={(patch) => handlePatch(t.id, patch)}
      onDelete={() => handleDelete(t)}
    />
  );

  return (
    <div className="center-body">
      {error && <div className="tree-error">{error}</div>}
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
                    <div className="empty-note">Пусто</div>
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

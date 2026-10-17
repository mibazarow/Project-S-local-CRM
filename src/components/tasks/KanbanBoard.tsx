import { useEffect, useMemo, useState, type DragEvent } from "react";
import * as ipc from "../../api/ipc";
import type { TaskDto, TaskPatch } from "../../api/types";
import { toMessage } from "../files/fileOps";
import RenameInput from "../files/RenameInput";
import { IconPlus } from "../files/icons";
import TaskCard from "./TaskCard";

const COLUMNS = [
  { status: "new", label: "Новые" },
  { status: "progress", label: "В работе" },
  { status: "done", label: "Готово" },
] as const;

interface Props {
  projectId: number;
  tasksVersion: number;
}

export default function KanbanBoard({ projectId, tasksVersion }: Props) {
  const [tasks, setTasks] = useState<TaskDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [draggingId, setDraggingId] = useState<number | null>(null);
  const [dropOverStatus, setDropOverStatus] = useState<string | null>(null);
  const [addingStatus, setAddingStatus] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    ipc.tasksList(projectId)
      .then((list) => {
        if (!cancelled) {
          setTasks(list);
          setError(null);
        }
      })
      .catch((e) => {
        if (!cancelled) setError(toMessage(e));
      });
    return () => {
      cancelled = true;
    };
  }, [projectId, tasksVersion]);

  const byStatus = useMemo(() => {
    const map: Record<string, TaskDto[]> = { new: [], progress: [], done: [] };
    for (const t of tasks ?? []) map[t.status]?.push(t);
    return map;
  }, [tasks]);

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

  const handleDragOver = (status: string, e: DragEvent<HTMLDivElement>) => {
    if (draggingId === null) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    setDropOverStatus(status);
  };

  const handleDragLeave = (e: DragEvent<HTMLDivElement>) => {
    if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDropOverStatus(null);
  };

  const handleDrop = (status: string, e: DragEvent<HTMLDivElement>) => {
    e.preventDefault();
    setDropOverStatus(null);
    const id = Number(e.dataTransfer.getData("text/plain"));
    if (!Number.isFinite(id)) return;

    const cards = Array.from(
      e.currentTarget.querySelectorAll<HTMLElement>(".task-card[data-id]"),
    ).filter((el) => Number(el.dataset.id) !== id);
    let index = cards.length;
    for (let i = 0; i < cards.length; i++) {
      const rect = cards[i].getBoundingClientRect();
      if (e.clientY < rect.top + rect.height / 2) {
        index = i;
        break;
      }
    }

    const others = byStatus[status].filter((t) => t.id !== id);
    let position: number;
    if (others.length === 0) position = 1;
    else if (index === 0) position = others[0].position - 1;
    else if (index >= others.length) position = others[others.length - 1].position + 1;
    else position = (others[index - 1].position + others[index].position) / 2;

    ipc
      .taskMove(id, status, position)
      .then(() => setError(null))
      .catch((e2) => setError(toMessage(e2)));
  };

  return (
    <div className="center-body">
      {error && <div className="tree-error">{error}</div>}
      {tasks && (
        <div className="task-board">
          {COLUMNS.map((col) => (
            <div
              key={col.status}
              className={dropOverStatus === col.status ? "task-col drop-over" : "task-col"}
              onDragOver={(e) => handleDragOver(col.status, e)}
              onDragLeave={handleDragLeave}
              onDrop={(e) => handleDrop(col.status, e)}
            >
              <div className="task-col-head">
                {col.label} <span className="count">{byStatus[col.status].length}</span>
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
                {byStatus[col.status].map((t) => (
                  <TaskCard
                    key={t.id}
                    task={t}
                    dragging={draggingId === t.id}
                    onDragStart={(e) => {
                      e.dataTransfer.setData("text/plain", String(t.id));
                      e.dataTransfer.effectAllowed = "move";
                      setDraggingId(t.id);
                    }}
                    onDragEnd={() => setDraggingId(null)}
                    onPatch={(patch) => handlePatch(t.id, patch)}
                    onDelete={() => handleDelete(t)}
                  />
                ))}
                {tasks && byStatus[col.status].length === 0 && addingStatus !== col.status && (
                  <div className="empty-note">Пусто</div>
                )}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

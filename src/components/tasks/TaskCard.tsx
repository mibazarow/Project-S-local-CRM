import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import type { TaskDto, TaskPatch } from "../../api/types";
import RenameInput from "../files/RenameInput";
import { IconCalendar, IconCheck, IconComment, IconPencil, IconTrash } from "../files/icons";

type Mode = "title" | "comment" | "due" | null;

interface Props {
  task: TaskDto;
  dragging: boolean;
  highlight?: boolean;
  onPointerDown: (e: PointerEvent<HTMLDivElement>) => void;
  onPatch: (patch: TaskPatch) => void;
  onDelete: () => void;
}

function formatDue(dueDate: string): string {
  const d = new Date(`${dueDate}T00:00:00`);
  if (Number.isNaN(d.getTime())) return dueDate;
  return d.toLocaleDateString("ru-RU", { day: "numeric", month: "short" });
}

function CommentInput({
  value,
  onCommit,
  onCancel,
}: {
  value: string;
  onCommit: (value: string) => void;
  onCancel: () => void;
}) {
  const ref = useRef<HTMLTextAreaElement>(null);
  const done = useRef(false);

  useEffect(() => {
    ref.current?.focus();
  }, []);

  const finish = (commit: boolean) => {
    if (done.current) return;
    done.current = true;
    if (commit) onCommit(ref.current?.value ?? "");
    else onCancel();
  };

  return (
    <textarea
      ref={ref}
      className="rename-input"
      defaultValue={value}
      placeholder="Комментарий…"
      rows={2}
      onClick={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        if (e.key === "Enter" && !e.shiftKey) {
          e.preventDefault();
          finish(true);
        }
        if (e.key === "Escape") finish(false);
      }}
      onBlur={() => finish(true)}
    />
  );
}

function DueEditor({
  task,
  onSave,
  onClear,
  onCancel,
}: {
  task: TaskDto;
  onSave: (dueDate: string | null, remindAt: string | null) => void;
  onClear: () => void;
  onCancel: () => void;
}) {
  const [date, setDate] = useState(task.dueDate ?? "");
  const [time, setTime] = useState(task.remindAt?.split("T")[1] ?? "");

  const save = () => {
    const due = date || null;
    onSave(due, due && time ? `${due}T${time}` : null);
  };

  const onInputKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter") save();
    if (e.key === "Escape") onCancel();
  };

  return (
    <div
      className="task-due-row"
      onMouseDown={(e) => e.stopPropagation()}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) save();
      }}
    >
      <input
        className="rename-input"
        type="date"
        value={date}
        onChange={(e) => setDate(e.target.value)}
        onKeyDown={onInputKeyDown}
      />
      <input
        className="rename-input"
        type="time"
        value={time}
        onChange={(e) => setTime(e.target.value)}
        onKeyDown={onInputKeyDown}
      />
      <button className="mini-btn" title="Сохранить дату" onClick={save}>
        <IconCheck />
      </button>
      <button className="mini-btn danger" title="Убрать дату" onClick={onClear}>
        <IconTrash />
      </button>
    </div>
  );
}

export default function TaskCard({
  task,
  dragging,
  highlight,
  onPointerDown,
  onPatch,
  onDelete,
}: Props) {
  const [mode, setMode] = useState<Mode>(null);
  const cardRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (highlight) cardRef.current?.scrollIntoView({ block: "center" });
  }, [highlight]);

  const classes = ["task-card"];
  if (task.status === "done") classes.push("done");
  if (task.overdue) classes.push("overdue");
  if (dragging) classes.push("dragging");
  if (mode) classes.push("editing");
  if (highlight) classes.push("highlight");

  const commitTitle = (title: string) => {
    setMode(null);
    if (title !== task.title) onPatch({ title });
  };

  const commitComment = (raw: string) => {
    setMode(null);
    const next = raw.trim();
    if (next === (task.comment ?? "")) return;
    // Empty must clear the comment: explicit null, not an absent field.
    onPatch(next ? { comment: next } : { comment: null });
  };

  const saveDue = (dueDate: string | null, remindAt: string | null) => {
    setMode(null);
    if (dueDate === task.dueDate && remindAt === task.remindAt) return;
    onPatch({ dueDate, remindAt });
  };

  const clearDue = () => {
    setMode(null);
    if (!task.dueDate && !task.remindAt) return;
    onPatch({ dueDate: null, remindAt: null });
  };

  return (
    <div
      ref={cardRef}
      className={classes.join(" ")}
      data-id={task.id}
      onPointerDown={mode === null ? onPointerDown : undefined}
    >
      {mode === "title" ? (
        <RenameInput
          value={task.title}
          onCommit={commitTitle}
          onCancel={() => setMode(null)}
        />
      ) : (
        <div className="t" onDoubleClick={() => setMode("title")}>
          {task.title}
        </div>
      )}
      {task.dueDate && (
        <div className={task.overdue ? "m red" : "m"}>{formatDue(task.dueDate)}</div>
      )}
      {mode === "due" && (
        <DueEditor
          task={task}
          onSave={saveDue}
          onClear={clearDue}
          onCancel={() => setMode(null)}
        />
      )}
      {task.comment && mode !== "comment" && <div className="c">{task.comment}</div>}
      {mode === "comment" && (
        <CommentInput
          value={task.comment ?? ""}
          onCommit={commitComment}
          onCancel={() => setMode(null)}
        />
      )}
      {task.tags.length > 0 && (
        <div className="tags">
          {task.tags.map((tag) => (
            <span key={tag} className="tag">
              {tag}
            </span>
          ))}
        </div>
      )}
      <span className="task-actions">
        <button
          className="mini-btn"
          title="Переименовать"
          onClick={(e) => {
            e.stopPropagation();
            setMode("title");
          }}
        >
          <IconPencil />
        </button>
        <button
          className="mini-btn"
          title="Комментарий"
          onClick={(e) => {
            e.stopPropagation();
            setMode("comment");
          }}
        >
          <IconComment />
        </button>
        <button
          className="mini-btn"
          title="Срок и напоминание"
          onClick={(e) => {
            e.stopPropagation();
            setMode("due");
          }}
        >
          <IconCalendar />
        </button>
        <button
          className="mini-btn danger"
          title="Удалить"
          onClick={(e) => {
            e.stopPropagation();
            onDelete();
          }}
        >
          <IconTrash />
        </button>
      </span>
    </div>
  );
}

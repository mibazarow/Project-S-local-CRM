import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import type { ChecklistItem, TaskDto, TaskPatch } from "../../api/types";
import { formatDue } from "../../shared/format";
import {
  IconAskAgent,
  IconCalendar,
  IconCheck,
  IconClose,
  IconComment,
  IconList,
  IconPencil,
  IconPlus,
  IconRepeat,
  IconTrash,
} from "../files/icons";

type Mode = "title" | "comment" | "due" | "details" | null;

const RECURRENCES = [
  { value: "none", label: "нет" },
  { value: "daily", label: "ежедневно" },
  { value: "weekly", label: "еженедельно" },
  { value: "monthly", label: "ежемесячно" },
];

const RECURRENCE_LABEL: Record<string, string> = Object.fromEntries(
  RECURRENCES.map((r) => [r.value, r.label]),
);

interface Props {
  task: TaskDto;
  dragging: boolean;
  highlight?: boolean;
  onPointerDown: (e: PointerEvent<HTMLDivElement>) => void;
  onPatch: (patch: TaskPatch) => void;
  onDelete: () => void;
  onDiscuss?: () => void;
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

function TitleInput({
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
    const el = ref.current;
    if (!el) return;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
  }, []);

  const finish = (commit: boolean) => {
    if (done.current) return;
    done.current = true;
    if (!commit) {
      onCancel();
      return;
    }
    const next = ref.current?.value.trim() ?? "";
    if (!next) onCancel();
    else onCommit(next);
  };

  return (
    <div className="task-title-edit" onMouseDown={(e) => e.stopPropagation()}>
      <textarea
        ref={ref}
        className="rename-input"
        defaultValue={value}
        rows={Math.min(6, Math.max(2, value.split("\n").length))}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            finish(true);
          }
          if (e.key === "Escape") {
            e.preventDefault();
            finish(false);
          }
        }}
      />
      <div className="task-title-edit-actions">
        <button
          className="mini-btn"
          title="Сохранить (⌘Enter)"
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => finish(true)}
        >
          <IconCheck />
        </button>
        <button className="mini-btn danger" title="Отменить" onClick={() => finish(false)}>
          <IconClose />
        </button>
      </div>
    </div>
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

function TaskDetails({
  task,
  onPatch,
  onCancel,
}: {
  task: TaskDto;
  onPatch: (patch: TaskPatch) => void;
  onCancel: () => void;
}) {
  const [newItem, setNewItem] = useState("");

  const setChecklist = (items: ChecklistItem[]) => onPatch({ checklist: items });

  const addItem = () => {
    const text = newItem.trim();
    if (!text) return;
    setNewItem("");
    setChecklist([...task.checklist, { text, done: false }]);
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") onCancel();
  };

  return (
    <div
      className="task-details"
      onMouseDown={(e) => e.stopPropagation()}
      onKeyDown={onKey}
    >
      <div className="td-row">
        <label className="tf-check td-priority">
          <input
            type="checkbox"
            checked={task.isPriority}
            onChange={(e) => onPatch({ isPriority: e.target.checked })}
          />
          Приоритетная
        </label>
        <label>
          Повтор
          <select
            value={task.recurrence}
            onChange={(e) => onPatch({ recurrence: e.target.value })}
          >
            {RECURRENCES.map((r) => (
              <option key={r.value} value={r.value}>
                {r.label}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="td-checklist">
        {task.checklist.map((item, i) => (
          <div key={i} className="td-check-row">
            <input
              type="checkbox"
              checked={item.done}
              onChange={() =>
                setChecklist(
                  task.checklist.map((x, j) => (j === i ? { ...x, done: !x.done } : x)),
                )
              }
            />
            <span className={item.done ? "strikethrough" : undefined}>{item.text}</span>
            <button
              className="mini-btn danger"
              title="Удалить пункт"
              onClick={() => setChecklist(task.checklist.filter((_, j) => j !== i))}
            >
              <IconClose />
            </button>
          </div>
        ))}
        <div className="td-check-add">
          <input
            className="rename-input"
            placeholder="Новый пункт…"
            value={newItem}
            onChange={(e) => setNewItem(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                addItem();
              }
            }}
          />
          <button className="mini-btn" title="Добавить пункт" onClick={addItem}>
            <IconPlus />
          </button>
        </div>
      </div>
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
  onDiscuss,
}: Props) {
  const [mode, setMode] = useState<Mode>(null);
  const cardRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (highlight) cardRef.current?.scrollIntoView({ block: "center" });
  }, [highlight]);

  const classes = ["task-card"];
  if (task.isPriority) classes.push("priority");
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
        <TitleInput
          value={task.title}
          onCommit={commitTitle}
          onCancel={() => setMode(null)}
        />
      ) : (
        <div className="t" onDoubleClick={() => setMode("title")}>
          {task.title}
        </div>
      )}
      {(task.dueDate || task.recurrence !== "none") && (
        <div className="m">
          {task.dueDate && (
            <span className={task.overdue ? "red" : undefined}>{formatDue(task.dueDate)}</span>
          )}
          {task.recurrence !== "none" && (
            <span className="rec" title="Повторяющаяся задача">
              <IconRepeat />
              {RECURRENCE_LABEL[task.recurrence] ?? task.recurrence}
            </span>
          )}
        </div>
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
      {task.checklist.length > 0 &&
        (() => {
          const doneCount = task.checklist.filter((i) => i.done).length;
          return (
            <div className="check-progress" title="Чек-лист">
              <span className="check-count">
                {doneCount}/{task.checklist.length}
              </span>
              <span className="check-bar">
                <i style={{ width: `${(doneCount / task.checklist.length) * 100}%` }} />
              </span>
            </div>
          );
        })()}
      {mode === "details" && (
        <TaskDetails task={task} onPatch={onPatch} onCancel={() => setMode(null)} />
      )}
      <span className="task-actions">
        {onDiscuss && (
          <button
            className="mini-btn"
            title="Обсудить с агентом"
            onClick={(e) => {
              e.stopPropagation();
              onDiscuss();
            }}
          >
            <IconAskAgent />
          </button>
        )}
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
          className="mini-btn"
          title="Приоритет, повтор, чек-лист"
          onClick={(e) => {
            e.stopPropagation();
            setMode("details");
          }}
        >
          <IconList />
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

import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import type { ChecklistItem, TaskDto, TaskPatch } from "../../api/types";
import { formatDue, formatSpent } from "../../shared/format";
import {
  IconAskAgent,
  IconClose,
  IconComment,
  IconPencil,
  IconPlus,
  IconTrash,
} from "../files/icons";

type Mode = "edit" | "comment" | null;

interface Props {
  task: TaskDto;
  dragging: boolean;
  highlight?: boolean;
  onPointerDown: (e: PointerEvent<HTMLDivElement>) => void;
  onPatch: (patch: TaskPatch) => void;
  onDelete: () => void;
  onDiscuss?: () => void;
}

export function CommentInput({
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

export function TaskEditor({
  task,
  onSave,
  onCancel,
}: {
  task: TaskDto;
  onSave: (patch: TaskPatch) => void;
  onCancel: () => void;
}) {
  const titleRef = useRef<HTMLTextAreaElement>(null);
  const [title, setTitle] = useState(task.title);
  const [comment, setComment] = useState(task.comment ?? "");
  const [dueDate, setDueDate] = useState(task.dueDate ?? "");
  const [dueTime, setDueTime] = useState(task.dueTime || "07:00");
  const [remindTime, setRemindTime] = useState(task.remindAt?.split("T")[1] ?? "");
  const [isPriority, setIsPriority] = useState(task.isPriority);
  const [checklist, setChecklist] = useState<ChecklistItem[]>(task.checklist);
  const [newItem, setNewItem] = useState("");

  useEffect(() => {
    titleRef.current?.focus();
  }, []);

  useEffect(() => {
    setIsPriority(task.isPriority);
  }, [task.isPriority]);

  const canSave = title.trim().length > 0;

  const addItem = () => {
    const text = newItem.trim();
    if (!text) return;
    setNewItem("");
    setChecklist((prev) => [...prev, { text, done: false }]);
  };

  const save = () => {
    if (!canSave) return;
    const date = dueDate || null;
    const next = comment.trim();
    onSave({
      title: title.trim(),
      comment: next || null,
      dueDate: date,
      dueTime: dueTime || "07:00",
      remindAt: date && remindTime ? `${date}T${remindTime}` : null,
      isPriority,
      checklist,
    });
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onCancel();
    }
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      save();
    }
  };

  return (
    <div
      className="task-details task-edit"
      onPointerDown={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
      onKeyDown={onKeyDown}
    >
      <textarea
        ref={titleRef}
        className="rename-input task-edit-title"
        placeholder="Название задачи"
        value={title}
        rows={Math.min(6, Math.max(2, title.split("\n").length))}
        onChange={(e) => setTitle(e.target.value)}
      />
      <textarea
        className="rename-input task-edit-comment"
        value={comment}
        placeholder="Комментарий…"
        rows={2}
        onChange={(e) => setComment(e.target.value)}
      />
      <div className="task-edit-datetime">
        <div className="task-due-row">
          <input
            className="rename-input"
            type="date"
            title="Срок"
            value={dueDate}
            onChange={(e) => setDueDate(e.target.value)}
          />
          <input
            className="rename-input"
            type="time"
            title="Время дедлайна"
            value={dueTime}
            onChange={(e) => setDueTime(e.target.value)}
          />
          <input
            className="rename-input"
            type="time"
            title="Время напоминания"
            value={remindTime}
            onChange={(e) => setRemindTime(e.target.value)}
          />
        </div>
        <label className="tf-check td-priority">
          <input
            type="checkbox"
            checked={isPriority}
            onChange={(e) => setIsPriority(e.target.checked)}
          />
          Важная
        </label>
      </div>
      <div className="td-checklist">
        {checklist.map((item, i) => (
          <div key={i} className="td-check-row">
            <input
              type="checkbox"
              checked={item.done}
              onChange={() =>
                setChecklist((prev) =>
                  prev.map((x, j) => (j === i ? { ...x, done: !x.done } : x)),
                )
              }
            />
            <span className={item.done ? "strikethrough" : undefined}>{item.text}</span>
            <button
              className="mini-btn danger"
              title="Удалить пункт"
              onClick={() => setChecklist((prev) => prev.filter((_, j) => j !== i))}
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
                e.stopPropagation();
                addItem();
              }
            }}
          />
          <button className="mini-btn" title="Добавить пункт" onClick={addItem}>
            <IconPlus />
          </button>
        </div>
      </div>
      <div className="task-edit-actions">
        <button className="mini-btn" title="Отменить" onClick={onCancel}>
          Отменить
        </button>
        <button className="mini-btn task-save-btn" title="Сохранить" disabled={!canSave} onClick={save}>
          Сохранить
        </button>
      </div>
    </div>
  );
}

function ChecklistView({
  items,
  onToggle,
}: {
  items: ChecklistItem[];
  onToggle: (index: number) => void;
}) {
  const doneCount = items.filter((i) => i.done).length;
  return (
    <div
      className="task-checklist"
      onPointerDown={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
      onClick={(e) => e.stopPropagation()}
    >
      <div className="task-checklist-head">
        <span>Чек-лист</span>
        <span className="check-count">
          {doneCount}/{items.length}
        </span>
      </div>
      <div className="td-checklist">
        {items.map((item, i) => (
          <label
            key={i}
            className="td-check-row"
            onPointerDown={(e) => e.stopPropagation()}
            onMouseDown={(e) => e.stopPropagation()}
          >
            <input type="checkbox" checked={item.done} onChange={() => onToggle(i)} />
            <span className={item.done ? "strikethrough" : undefined}>{item.text}</span>
          </label>
        ))}
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

  // Wall-clock timer footer (SPEC 4.3): while the task sits in "progress"
  // the total ticks once per second from the epoch-ms start stamp, so the
  // count keeps running even when the app was closed. The state lives in
  // this card only — the rest of the board is not re-rendered per tick.
  const running = task.status === "progress" && task.startedAt !== null;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!running) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [running]);
  const spentMs = task.spentMs + (running ? Math.max(0, now - (task.startedAt as number)) : 0);

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

  const commitComment = (raw: string) => {
    setMode(null);
    const next = raw.trim();
    if (next === (task.comment ?? "")) return;
    // Empty must clear the comment: explicit null, not an absent field.
    onPatch(next ? { comment: next } : { comment: null });
  };

  const toggleChecklist = (index: number) => {
    onPatch({
      checklist: task.checklist.map((x, j) => (j === index ? { ...x, done: !x.done } : x)),
    });
  };

  const saveAll = (patch: TaskPatch) => {
    setMode(null);
    onPatch(patch);
  };

  return (
    <div
      ref={cardRef}
      className={classes.join(" ")}
      data-id={task.id}
      onPointerDown={mode === null ? onPointerDown : undefined}
    >
      {mode === "edit" ? (
        <TaskEditor task={task} onSave={saveAll} onCancel={() => setMode(null)} />
      ) : (
        <>
          <div className="t" onDoubleClick={() => setMode("edit")}>
            {task.title}
          </div>
          {task.overdue && <div className="overdue-label">Просрочено</div>}
          {task.dueDate && (
            <div className="m">
              <span className={task.overdue ? "red" : undefined}>
                {formatDue(task.dueDate)}
              </span>
              <span className={task.overdue ? "red" : "due-time"}>{task.dueTime}</span>
            </div>
          )}
          {task.comment && mode !== "comment" && <div className="c">{task.comment}</div>}
          {mode === "comment" && (
            <CommentInput
              value={task.comment ?? ""}
              onCommit={commitComment}
              onCancel={() => setMode(null)}
            />
          )}
          {task.checklist.length > 0 && (
            <ChecklistView items={task.checklist} onToggle={toggleChecklist} />
          )}
        </>
      )}
      <div className="task-foot">
        {(running || spentMs > 0) && (
          <div className="task-spent">Затрачено: {formatSpent(spentMs)}</div>
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
            title="Редактировать"
            onClick={(e) => {
              e.stopPropagation();
              setMode("edit");
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
    </div>
  );
}

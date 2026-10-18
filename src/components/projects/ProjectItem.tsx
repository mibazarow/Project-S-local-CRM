import { useState, type PointerEvent } from "react";
import type { ProjectDto } from "../../api/types";

interface Props {
  project: ProjectDto;
  active: boolean;
  dragging: boolean;
  onOpen: (id: number) => void;
  onRename: (id: number, newName: string) => void;
  onPointerDown: (e: PointerEvent<HTMLDivElement>) => void;
}

export default function ProjectItem({
  project,
  active,
  dragging,
  onOpen,
  onRename,
  onPointerDown,
}: Props) {
  const [renaming, setRenaming] = useState(false);
  const [draft, setDraft] = useState(project.name);

  const startRename = () => {
    setDraft(project.name);
    setRenaming(true);
  };

  const commit = () => {
    setRenaming(false);
    const next = draft.trim();
    if (next && next !== project.name) onRename(project.id, next);
  };

  const classes = ["project-item"];
  if (active) classes.push("active");
  if (dragging) classes.push("dragging");

  return (
    <div
      className={classes.join(" ")}
      role="button"
      tabIndex={0}
      data-id={project.id}
      onPointerDown={renaming ? undefined : onPointerDown}
      onClick={() => !renaming && onOpen(project.id)}
    >
      {renaming ? (
        <input
          className="rename-input"
          value={draft}
          autoFocus
          onClick={(e) => e.stopPropagation()}
          onMouseDown={(e) => e.stopPropagation()}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
            if (e.key === "Escape") setRenaming(false);
          }}
        />
      ) : (
        <span className="name">{project.name}</span>
      )}
      <span className="meta">
        {project.fileCount} файлов · {project.taskCount} задач
        {project.overdueCount > 0 && (
          <>
            {" · "}
            <span className="badge-red">{project.overdueCount} просрочено</span>
          </>
        )}
        {project.hasActiveTasks && (
          <svg
            className="active-tasks"
            viewBox="0 0 24 24"
            width="11"
            height="11"
            fill="currentColor"
            role="img"
            aria-label="Есть задачи в работе"
          >
            <title>Есть задачи в работе</title>
            <path d="M8 5v14l11-7z" />
          </svg>
        )}
      </span>
      <span className="proj-actions">
        <button
          className="mini-btn"
          title="Переименовать проект"
          onClick={(e) => {
            e.stopPropagation();
            startRename();
          }}
        >
          <svg
            viewBox="0 0 24 24"
            width="14"
            height="14"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z" />
          </svg>
        </button>
      </span>
    </div>
  );
}

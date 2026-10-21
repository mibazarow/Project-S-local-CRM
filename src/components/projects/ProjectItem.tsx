import { useState, type MouseEvent, type PointerEvent } from "react";
import type { ProjectDto } from "../../api/types";
import { projectKey, useChatReady, useChatRunning } from "../../state/chatRunStore";

interface Props {
  project: ProjectDto;
  active: boolean;
  dragging: boolean;
  onOpen: (id: number) => void;
  onRename: (id: number, newName: string) => void;
  onArchive: (id: number, archived: boolean) => void;
  onDelete: (id: number) => void;
  onContextMenu: (e: MouseEvent<HTMLDivElement>) => void;
  onPointerDown: (e: PointerEvent<HTMLDivElement>) => void;
}

export default function ProjectItem({
  project,
  active,
  dragging,
  onOpen,
  onRename,
  onArchive,
  onDelete,
  onContextMenu,
  onPointerDown,
}: Props) {
  const [renaming, setRenaming] = useState(false);
  const [draft, setDraft] = useState(project.name);
  const running = useChatRunning(projectKey(project.id));
  const ready = useChatReady(projectKey(project.id));

  const startRename = () => {
    setDraft(project.name);
    setRenaming(true);
  };

  const commit = () => {
    setRenaming(false);
    const next = draft.trim();
    if (next && next !== project.name) onRename(project.id, next);
  };

  const pluralTasks = (n: number) => {
  const mod10 = n % 10;
  const mod100 = n % 100;
  if (mod10 === 1 && mod100 !== 11) return "задача";
  if (mod10 >= 2 && mod10 <= 4 && (mod100 < 12 || mod100 > 14)) return "задачи";
  return "задач";
};

const classes = ["project-item"];
  if (active) classes.push("active");
  if (dragging) classes.push("dragging");
  if (project.builtin) classes.push("builtin");

  return (
    <div
      className={classes.join(" ")}
      role="button"
      tabIndex={0}
      data-id={project.id}
      onPointerDown={renaming ? undefined : onPointerDown}
      onContextMenu={onContextMenu}
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
        <span className="name">
          {running && <span className="chat-status running" title="Агент работает" />}
          {!running && ready && <span className="chat-status ready" title="Ответ готов" />}
          {project.name}
        </span>
      )}
      <span className="meta">
        {project.builtin
          ? `${project.taskCount} задач`
          : `${project.fileCount} файлов · ${project.taskCount} задач`}
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
      {project.overdueCount > 0 && (
        <span className="meta meta-overdue">
          Просрочено {project.overdueCount} {pluralTasks(project.overdueCount)}
        </span>
      )}
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
        {!project.builtin && (
          <button
            className="mini-btn archive-toggle-btn"
            title={project.archived ? "Вернуть из архива" : "В архив"}
            onClick={(e) => {
              e.stopPropagation();
              onArchive(project.id, !project.archived);
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
              <rect x="2" y="3" width="20" height="5" rx="1" />
              <path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8" />
              {project.archived ? (
                <>
                  <path d="M12 12v6" />
                  <path d="m9 15 3-3 3 3" />
                </>
              ) : (
                <path d="M10 12h4" />
              )}
            </svg>
            <span className="btn-label">{project.archived ? "Вернуть" : "В архив"}</span>
          </button>
        )}
        {!project.builtin && (
          <button
            className="mini-btn danger delete-btn"
            title="Удалить проект"
            onClick={(e) => {
              e.stopPropagation();
              onDelete(project.id);
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
              <path d="M3 6h18" />
              <path d="M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2" />
              <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
              <path d="M10 11v6" />
              <path d="M14 11v6" />
            </svg>
          </button>
        )}
      </span>
    </div>
  );
}

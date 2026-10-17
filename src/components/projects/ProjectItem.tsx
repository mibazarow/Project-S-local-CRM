import { useState } from "react";
import type { ProjectDto } from "../../api/types";

interface Props {
  project: ProjectDto;
  active: boolean;
  onOpen: (id: number) => void;
  onRename: (id: number, newName: string) => void;
}

export default function ProjectItem({ project, active, onOpen, onRename }: Props) {
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

  return (
    <div
      className={active ? "project-item active" : "project-item"}
      role="button"
      tabIndex={0}
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
